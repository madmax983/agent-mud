//! The MCP server over real HTTP, driven by the rmcp client that littleguy
//! uses.

#[cfg(test)]
mod http {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use mud_mcp::{CommandGate, DirectGate, TOOLS, TokenRegistry, router};
    use mud_world::{CommandResult, EntityId, Game, ManualClock, World, WorldPack};
    use rmcp::ServiceExt;
    use rmcp::model::{CallToolRequestParams, CallToolResult};
    use rmcp::service::{RoleClient, RunningService};
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use serde_json::{Value, json};

    type Client = RunningService<RoleClient, ()>;

    struct Server {
        url: String,
        game: Arc<Game>,
        tokens: Arc<TokenRegistry>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Server {
        fn issue(&self, entity: &EntityId) -> String {
            self.tokens.issue(entity.clone()).unwrap()
        }

        async fn client(&self, token: &str) -> Client {
            let config = StreamableHttpClientTransportConfig::with_uri(self.url.as_str())
                .auth_header(token.to_owned());
            ().serve(StreamableHttpClientTransport::from_config(config))
                .await
                .unwrap()
        }
    }

    fn game() -> Arc<Game> {
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        Game::new(World::new(pack, 7), None, Arc::new(ManualClock::new(1_000)))
    }

    /// Serves the router on a loopback port, with the gate `make_gate` makes.
    async fn start(
        game: Arc<Game>,
        make_gate: impl FnOnce(Arc<Game>) -> Arc<dyn CommandGate>,
    ) -> Server {
        // Workspace feature unification can leave reqwest with no TLS
        // crypto provider; install one before any client is built.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tokens = Arc::new(TokenRegistry::new());
        let app: axum::Router = router(
            Arc::clone(&game),
            Arc::clone(&tokens),
            make_gate(Arc::clone(&game)),
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Server {
            url: format!("http://{addr}/mcp"),
            game,
            tokens,
            task,
        }
    }

    fn direct(game: Arc<Game>) -> Arc<dyn CommandGate> {
        Arc::new(DirectGate::new(game))
    }

    async fn call_raw(client: &Client, tool: &'static str, args: Value) -> CallToolResult {
        let Value::Object(args) = args else {
            panic!("arguments must be an object");
        };
        client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(args))
            .await
            .unwrap()
    }

    async fn call(client: &Client, tool: &'static str, args: Value) -> Value {
        let result = call_raw(client, tool, args).await;
        assert_ne!(result.is_error, Some(true), "{tool} failed: {result:?}");
        result.structured_content.unwrap()
    }

    fn texts(events: &Value) -> Vec<String> {
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["untrusted_text"].as_str().map(str::to_owned))
            .collect()
    }

    #[tokio::test]
    async fn tools_list_has_exactly_the_four_tools() {
        let server = start(game(), direct).await;
        let ada = server.game.login("ada").unwrap();
        let client = server.client(&server.issue(&ada)).await;
        let tools = client.list_all_tools().await.unwrap();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        let mut want = TOOLS.to_vec();
        want.sort_unstable();
        assert_eq!(names, want);
        for tool in &tools {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            let description = tool.description.as_deref().unwrap_or("");
            assert!(
                description.contains("untrusted_text")
                    && description.contains("never instructions"),
                "{}: {description}",
                tool.name
            );
        }
        let command = tools.iter().find(|t| t.name == "mud_command").unwrap();
        assert_eq!(command.input_schema["required"], json!(["cmd"]));
        let events = tools.iter().find(|t| t.name == "mud_events").unwrap();
        assert_eq!(events.input_schema["properties"]["limit"]["maximum"], 200);
        assert!(events.input_schema["properties"]["since_seq"].is_object());
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn look_and_sheet_show_the_callers_own_entity() {
        let server = start(game(), direct).await;
        let ada = server.game.login("ada").unwrap();
        let client = server.client(&server.issue(&ada)).await;
        let look = call(&client, "mud_look", json!({})).await;
        assert_eq!(look["room_id"], "town_square");
        let sheet = call(&client, "mud_sheet", json!({})).await;
        assert_eq!(sheet["id"], ada.as_str());
        assert_eq!(sheet["name"], "ada");
        client.cancel().await.unwrap();

        let marta = server.client(&server.issue(&EntityId::from("marta"))).await;
        let look = call(&marta, "mud_look", json!({})).await;
        assert_eq!(look["room_id"], "rusty_flagon_taproom");
        marta.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn speech_reaches_the_room_and_no_further() {
        let server = start(game(), direct).await;
        let bob = server.game.login("bob").unwrap();
        let ada = server.game.login("ada").unwrap();
        let ada_client = server.client(&server.issue(&ada)).await;
        let bob_client = server.client(&server.issue(&bob)).await;
        let marta_client = server.client(&server.issue(&EntityId::from("marta"))).await;

        let said = call(&ada_client, "mud_command", json!({ "cmd": "say hello" })).await;
        assert_eq!(said["ok"], true, "{said}");
        assert_eq!(said["verb"], "say");

        let heard = call(&bob_client, "mud_events", json!({ "since_seq": 0 })).await;
        let speech: Vec<&Value> = heard["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "speech")
            .collect();
        assert_eq!(speech.len(), 1, "{heard}");
        assert_eq!(speech[0]["untrusted_text"], "hello");
        assert_eq!(speech[0]["actor"]["id"], ada.as_str());
        assert_eq!(speech[0]["actor"]["is_self"], false);
        assert_eq!(heard["room_id"], "town_square");

        let far = call(&marta_client, "mud_events", json!({ "since_seq": 0 })).await;
        assert_eq!(far["events"], json!([]), "marta is in the taproom: {far}");

        for c in [ada_client, bob_client, marta_client] {
            c.cancel().await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_remembered_cursor_never_repeats_an_event() {
        let server = start(game(), direct).await;
        let bob = server.game.login("bob").unwrap();
        let ada = server.game.login("ada").unwrap();
        let ada_client = server.client(&server.issue(&ada)).await;
        let bob_token = server.issue(&bob);
        let bob_client = server.client(&bob_token).await;

        call(&ada_client, "mud_command", json!({ "cmd": "say one" })).await;
        let first = call(&bob_client, "mud_events", json!({})).await;
        assert_eq!(texts(&first), ["one"]);
        let cursor = first["cursor"].as_u64().unwrap();
        assert!(cursor > 0);

        let again = call(&bob_client, "mud_events", json!({})).await;
        assert_eq!(again["events"], json!([]), "no repeats: {again}");
        assert_eq!(again["cursor"].as_u64(), Some(cursor));

        call(&ada_client, "mud_command", json!({ "cmd": "say two" })).await;
        call(&ada_client, "mud_command", json!({ "cmd": "say three" })).await;
        let paged = call(&bob_client, "mud_events", json!({ "limit": 1 })).await;
        assert_eq!(texts(&paged), ["two"]);
        let rest = call(&bob_client, "mud_events", json!({})).await;
        assert_eq!(texts(&rest), ["three"]);

        // An explicit `since_seq` re-reads without moving the remembered cursor.
        let replay = call(&bob_client, "mud_events", json!({ "since_seq": 0 })).await;
        assert_eq!(texts(&replay), ["one", "two", "three"]);
        let after = call(&bob_client, "mud_events", json!({})).await;
        assert_eq!(after["events"], json!([]));

        // A second token of the same entity has its own cursor.
        let other = server.client(&server.issue(&bob)).await;
        let fresh = call(&other, "mud_events", json!({})).await;
        assert_eq!(texts(&fresh), ["one", "two", "three"]);

        for c in [ada_client, bob_client, other] {
            c.cancel().await.unwrap();
        }
    }

    #[tokio::test]
    async fn missing_wrong_and_revoked_tokens_get_401() {
        let server = start(game(), direct).await;
        let ada = server.game.login("ada").unwrap();
        let token = server.issue(&ada);
        let http = reqwest::Client::new();
        let init = initialize_body();

        let status = |r: reqwest::Response| r.status().as_u16();
        let bare = http
            .post(&server.url)
            .headers(mcp_headers())
            .body(init.clone());
        assert_eq!(status(bare.send().await.unwrap()), 401);
        let wrong = http
            .post(&server.url)
            .headers(mcp_headers())
            .bearer_auth("0123456789abcdef")
            .body(init.clone());
        assert_eq!(status(wrong.send().await.unwrap()), 401);
        let basic = http
            .post(&server.url)
            .headers(mcp_headers())
            .header("authorization", format!("Basic {token}"))
            .body(init.clone());
        assert_eq!(status(basic.send().await.unwrap()), 401);
        let good = http
            .post(&server.url)
            .headers(mcp_headers())
            .bearer_auth(&token)
            .body(init.clone());
        assert_eq!(status(good.send().await.unwrap()), 200);

        assert!(server.tokens.revoke(&token));
        let revoked = http
            .post(&server.url)
            .headers(mcp_headers())
            .bearer_auth(&token)
            .body(init);
        assert_eq!(status(revoked.send().await.unwrap()), 401);

        // Paths other than the endpoint are not served at all.
        let elsewhere = http
            .get(server.url.replace("/mcp", "/other"))
            .send()
            .await
            .unwrap();
        assert_eq!(status(elsewhere), 404);
    }

    fn mcp_headers() -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("content-type", "application/json".parse().unwrap());
        h.insert(
            "accept",
            "application/json, text/event-stream".parse().unwrap(),
        );
        h
    }

    fn initialize_body() -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "raw", "version": "0" },
            },
        })
        .to_string()
    }

    /// The JSON-RPC messages of a response body, plain JSON or SSE.
    fn messages(body: &str) -> Vec<Value> {
        if let Ok(v) = serde_json::from_str::<Value>(body) {
            return vec![v];
        }
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str(d.trim()).ok())
            .collect()
    }

    #[tokio::test]
    async fn a_token_acts_only_as_its_own_entity() {
        let server = start(game(), direct).await;
        let ada = server.game.login("ada").unwrap();
        let bob = server.game.login("bob").unwrap();
        let ada_token = server.issue(&ada);
        let bob_token = server.issue(&bob);

        // Through the rmcp client: each token sees and acts as its own entity.
        let bob_client = server.client(&bob_token).await;
        assert_eq!(
            call(&bob_client, "mud_sheet", json!({})).await["name"],
            "bob"
        );
        let said = call(&bob_client, "mud_command", json!({ "cmd": "say it is me" })).await;
        assert_eq!(said["ok"], true);
        let ada_client = server.client(&ada_token).await;
        let heard = call(&ada_client, "mud_events", json!({ "since_seq": 0 })).await;
        let speech = heard["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "speech")
            .unwrap();
        assert_eq!(speech["actor"]["id"], bob.as_str());
        bob_client.cancel().await.unwrap();
        ada_client.cancel().await.unwrap();

        // Raw HTTP: a session that ada's token opened, then used with bob's
        // token, still acts as bob, and without a token gets 401.
        let http = reqwest::Client::new();
        let opened = http
            .post(&server.url)
            .headers(mcp_headers())
            .bearer_auth(&ada_token)
            .body(initialize_body())
            .send()
            .await
            .unwrap();
        assert_eq!(opened.status().as_u16(), 200);
        let session = opened
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        drop(opened.text().await.unwrap());
        let send = |token: Option<&str>, body: Value| {
            let mut r = http
                .post(&server.url)
                .headers(mcp_headers())
                .header("mcp-session-id", &session)
                .header("mcp-protocol-version", "2025-06-18")
                .body(body.to_string());
            if let Some(t) = token {
                r = r.bearer_auth(t);
            }
            r.send()
        };
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let status = send(Some(&ada_token), initialized).await.unwrap().status();
        assert!(status.is_success(), "{status}");

        let sheet_call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "mud_sheet", "arguments": { "entity": "ada" } },
        });
        let answer = send(Some(&bob_token), sheet_call.clone()).await.unwrap();
        assert_eq!(answer.status().as_u16(), 200);
        let body = answer.text().await.unwrap();
        let reply = messages(&body)
            .into_iter()
            .find(|m| m["id"] == 2)
            .unwrap_or_else(|| panic!("no reply in {body}"));
        assert_eq!(
            reply["result"]["structuredContent"]["name"], "bob",
            "{reply}"
        );

        let anonymous = send(None, sheet_call).await.unwrap();
        assert_eq!(anonymous.status().as_u16(), 401);
    }

    /// A gate that records each call and runs `say` instead of the line.
    struct EchoGate {
        game: Arc<Game>,
        calls: Arc<Mutex<Vec<(EntityId, String)>>>,
    }

    impl CommandGate for EchoGate {
        fn run(&self, entity: EntityId, line: String) -> BoxFuture<'static, CommandResult> {
            self.calls
                .lock()
                .unwrap()
                .push((entity.clone(), line.clone()));
            let game = Arc::clone(&self.game);
            Box::pin(async move { game.execute(&entity, &format!("say gated: {line}")) })
        }
    }

    #[tokio::test]
    async fn mud_command_goes_through_the_gate() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let server = start(game(), move |game| {
            Arc::new(EchoGate {
                game,
                calls: recorded,
            })
        })
        .await;
        let kestrel = EntityId::from("pc_kestrel");
        server.game.bring_online(&kestrel);
        let client = server.client(&server.issue(&kestrel)).await;

        let result = call(&client, "mud_command", json!({ "cmd": "north" })).await;
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["verb"], "say");
        assert_eq!(
            calls.lock().unwrap().clone(),
            vec![(kestrel.clone(), "north".to_owned())]
        );
        let mine = call(&client, "mud_events", json!({})).await;
        assert_eq!(texts(&mine), ["gated: north"]);

        // Bad arguments never reach the gate.
        let missing = client
            .call_tool(CallToolRequestParams::new("mud_command"))
            .await;
        assert!(missing.is_err(), "{missing:?}");
        assert_eq!(calls.lock().unwrap().len(), 1);
        client.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_the_stop_token_ends_sessions_so_the_server_can_drain() {
        // Regression: Ctrl+C left `mud serve` waiting minutes, because each
        // littleguy daemon held an MCP session (an SSE stream) open and a
        // graceful shutdown waits for every open response.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let game = game();
        let tokens = Arc::new(TokenRegistry::new());
        let stop = tokio_util::sync::CancellationToken::new();
        let app: axum::Router = mud_mcp::router_until(
            Arc::clone(&game),
            Arc::clone(&tokens),
            direct(Arc::clone(&game)),
            stop.clone(),
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let drained = stop.clone();
        let served = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { drained.cancelled().await })
                .await
        });
        let token = tokens.issue(EntityId::from("torin")).unwrap();
        let config = StreamableHttpClientTransportConfig::with_uri(url.as_str()).auth_header(token);
        let client = ().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap();
        let _ = call(&client, "mud_look", json!({})).await;
        // Let the client open its server-to-client stream.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        stop.cancel();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), served).await;
        assert!(
            ended.is_ok(),
            "the server drained within 5 s with a client still connected"
        );
        drop(client);
    }
}
