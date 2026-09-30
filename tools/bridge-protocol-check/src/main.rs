//! Codec/transport fixture, NOT a simulator or an OMSI compatibility implementation.
//! Uses the game's exact protocol source so checks do not test a second implementation.
#[path = "../../../crates/omsi-app/src/tangenta_bridge/protocol.rs"]
mod protocol;
#[cfg(test)]
#[path = "../../../crates/omsi-app/src/tangenta_bridge/command.rs"]
mod command;

use serde_json::{json, Value};
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = std::env::args().nth(1).map(PathBuf::from)
        .ok_or("usage: openomsi-bridge-protocol-check <temporary-manifest-path>")?;
    if manifest.exists() { return Err("refusing to overwrite an existing manifest".into()); }
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let endpoint = json!({"protocol":1,"pid":std::process::id(),"port":listener.local_addr()?.port(),
        "token":"fixture-test-token-only","session_id":"fixture-session"});
    std::fs::write(&manifest, serde_json::to_vec(&endpoint)?)?;
    eprintln!("Protocol fixture ready; it does not connect to the game.");
    let mut snapshot = json!({"session_id":"fixture-session","sequence":1,
        "capabilities":["variables","script_texture"],"map_name":"Synthetic protocol fixture",
        "clock":{"hour":12,"minute":34,"second":56,"day":1,"month":1,"year":2026,"service_seconds":45296},
        "vehicle":{"id":1,"generation":1,"file_name":"fixture.bus","friendly_name":"Protocol fixture",
            "tile_index":4,"tile_x":2,"tile_y":3,"position":{"x":1.5,"y":2.5,"z":3.5},
            "heading":0.5,"brightness":0.75,"variables":{"fixture_number":1.0},
            "strings":{"fixture_string":"initial"},"schedule":{"active":false},"hof":null}});
    let result = (|| -> io::Result<()> {
        for incoming in listener.incoming() {
            let mut stream = incoming?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            while let Ok((request, binary)) = protocol::read_frame(&mut stream) {
                if request.protocol != 1 || !protocol::authenticated("fixture-test-token-only", &request.token) { break; }
                let mut response = json!({"protocol":1,"request_id":request.request_id,"ok":true});
                let result = match request.op.as_str() {
                    "snapshot" => { response["snapshot"] = snapshot.clone(); Ok(()) }
                    "fixture_shutdown" => {
                        protocol::write_response(&mut stream, &response)?;
                        return Ok(());
                    }
                    _ if request.session_id != "fixture-session" || request.vehicle_id != 1 || request.generation != 1 =>
                        Err("stale fixture identity".into()),
                    "set_variables" => protocol::validate_values(&request).map(|()| {
                        for (name, value) in &request.values { snapshot["vehicle"]["variables"][name] = json!(value); }
                        for (name, value) in &request.strings { snapshot["vehicle"]["strings"][name] = json!(value); }
                        snapshot["sequence"] = json!(snapshot["sequence"].as_u64().unwrap() + 1);
                    }),
                    "script_texture" => protocol::decode_bgra(&request, binary).map(|rgba| {
                        // Inspection available only in this synthetic fixture, never the game.
                        snapshot["fixture_texture_bytes"] = json!(rgba.len());
                        snapshot["fixture_first_pixel"] = json!(rgba.get(..4));
                    }),
                    _ => Err("unsupported fixture operation".into()),
                };
                if let Err(error) = result {
                    response["ok"] = Value::Bool(false);
                    response["error"] = Value::String(error);
                }
                protocol::write_response(&mut stream, &response)?;
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(manifest);
    result?;
    Ok(())
}
