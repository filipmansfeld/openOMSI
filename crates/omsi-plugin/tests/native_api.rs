//! Exercise the new Lua boundary through the actual plugin lifecycle, including a
//! synchronous write/read and a rejected stale handle with no partial write.
use omsi_plugin::{HostConfig, PluginIo, Plugins};
use serde_json::{json, Value};

#[derive(Default)]
struct Game {
    value: f32,
    writes: usize,
    messages: Vec<String>,
}

impl PluginIo for Game {
    fn api(&mut self, operation: &str, args: Value, _binary: &[u8]) -> Result<Value, String> {
        match operation {
            "vehicle.read" => Ok(json!({"id":"vehicle:7", "generation":3, "value":self.value})),
            "vehicle.write" => {
                if args["id"] != "vehicle:7" || args["generation"] != 3 {
                    return Err("stale vehicle handle".into());
                }
                let value = args["value"].as_f64().ok_or("missing value")? as f32;
                self.value = value;
                self.writes += 1;
                Ok(json!({"applied":true}))
            }
            _ => Err(format!("unsupported operation: {operation}")),
        }
    }
    fn system(&mut self, _: &str) -> Option<f32> {
        None
    }
    fn set_system(&mut self, _: &str, _: f32) {}
    fn has_vehicle(&self) -> bool {
        true
    }
    fn var(&mut self, name: &str) -> Option<f32> {
        (name == "test").then_some(self.value)
    }
    fn set_var(&mut self, _: &str, value: f32) {
        self.value = value;
    }
    fn string(&mut self, _: &str) -> Option<String> {
        None
    }
    fn set_string(&mut self, _: &str, _: &str) {}
    fn fire(&mut self, _: &str, _: bool) {}
    fn dt(&self) -> f32 {
        0.016
    }
    fn message(&mut self, text: &str, _: f32) {
        self.messages.push(text.into());
    }
}

#[test]
fn native_writes_are_visible_in_the_same_callback() {
    let folder = std::env::temp_dir().join(format!(
        "openomsi-native-api-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("contract.lua"),
        r#"
        function on_frame()
            local original = omsi.api("vehicle.read")
            local reply = omsi.api("vehicle.write", {
                id = original.id, generation = original.generation, value = 42.5
            })
            assert(reply.applied == true)
            assert(omsi.api("vehicle.read").value == 42.5)
            assert(omsi.var("test") == 42.5)
            local ok, err = pcall(omsi.api, "vehicle.write", {
                id = original.id, generation = original.generation - 1, value = 999
            })
            assert(not ok and string.find(tostring(err), "stale vehicle handle", 1, true))
            assert(omsi.api("vehicle.read").value == 42.5)
            assert(io == nil and os.execute == nil and package.loadlib == nil)
        end
    "#,
    )
    .unwrap();
    let mut plugins = Plugins::load(&[folder.clone()], &HostConfig::default());
    assert_eq!(plugins.lua.len(), 1);
    let mut game = Game::default();
    plugins.frame(&mut game);
    assert_eq!(game.writes, 1);
    assert_eq!(game.value, 42.5);
    assert!(game.messages.is_empty(), "{:?}", game.messages);
    plugins.finalize();
    std::fs::remove_file(folder.join("contract.lua")).unwrap();
    std::fs::remove_dir(folder).unwrap();
}
