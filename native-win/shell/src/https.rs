// Minimal blocking client for the local server (127.0.0.1 only). Mirrors the
// URLSession posts in AppHost.swift: fire-and-forget commands with a short
// timeout, plus the /api/state poll that drives language (and, in M3, key
// configuration and audio inventory) synchronization.
use std::time::Duration;

use serde_json::Value;
use ureq::Agent;

fn agent(timeout: Duration) -> Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

pub fn post_command(base: &str, token: &str, body: &Value, timeout: Duration) {
    let request = agent(timeout).post(&format!("{}/api/command", base));
    let request = if token.is_empty() { request } else { request.set("X-Air-Token", token) };
    let _ = request
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
}

pub fn get_state(base: &str, token: &str, timeout: Duration) -> Option<Value> {
    let request = agent(timeout).get(&format!("{}/api/state", base));
    let request = if token.is_empty() { request } else { request.set("X-Air-Token", token) };
    let response = request.call().ok()?;
    let text = response.into_string().ok()?;
    serde_json::from_str(&text).ok()
}
