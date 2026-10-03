//! Bounded conversion between native game data and ordinary Lua tables. This does not
//! expose process memory, filesystem access, native modules, or arbitrary game pointers.

use mlua::{Lua, Table, Value};
use serde_json::{Map, Number, Value as Json};
use std::collections::BTreeMap;

const MAX_DEPTH: usize = 24;
const MAX_NODES: usize = 65_536;
const MAX_TEXT: usize = 2 * 1024 * 1024;
const MAX_BINARY: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Budget {
    nodes: usize,
    text: usize,
}

impl Budget {
    fn enter(&mut self, depth: usize) -> mlua::Result<()> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(mlua::Error::runtime(
                "native API data is too deep or too large (cyclic tables are not supported)",
            ));
        }
        Ok(())
    }

    fn text(&mut self, size: usize) -> mlua::Result<()> {
        self.text = self
            .text
            .checked_add(size)
            .ok_or_else(|| mlua::Error::runtime("native API text limit exceeded"))?;
        if self.text > MAX_TEXT {
            return Err(mlua::Error::runtime(
                "native API text limit exceeded (2 MiB)",
            ));
        }
        Ok(())
    }
}

fn to_json(
    value: Value,
    depth: usize,
    budget: &mut Budget,
    null: &Table,
    array_meta: &Table,
) -> mlua::Result<Json> {
    budget.enter(depth)?;
    Ok(match value {
        Value::Nil => Json::Null,
        Value::Boolean(b) => Json::Bool(b),
        Value::Integer(n) => Json::Number(n.into()),
        Value::Number(n) => Json::Number(Number::from_f64(n).ok_or_else(|| mlua::Error::runtime("native API numbers must be finite"))?),
        Value::String(s) => {
            budget.text(s.as_bytes().len())?;
            Json::String(s.to_str()?.to_string())
        }
        Value::Table(table) => {
            if table.to_pointer() == null.to_pointer() { return Ok(Json::Null); }
            // Read metatable identity through mlua's raw API. Neither __index nor
            // __pairs/__eq user code is invoked by marker inspection or iteration.
            let explicit_array = table.metatable().is_some_and(|meta|meta.to_pointer()==array_meta.to_pointer());
            let mut object = Map::new();
            let mut array = BTreeMap::new();
            // pairs reads raw table entries rather than invoking user-defined index code.
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                let value = to_json(value, depth + 1, budget, null, array_meta)?;
                match key {
                    Value::String(s) => {
                        budget.text(s.as_bytes().len())?;
                        object.insert(s.to_str()?.to_string(), value);
                    }
                    Value::Integer(i) if i > 0 && i <= MAX_NODES as i64 => {
                        array.insert(i as usize, value);
                    }
                    _ => return Err(mlua::Error::runtime("native API table keys must be strings or consecutive positive integers")),
                }
            }
            if !object.is_empty() && (explicit_array || !array.is_empty()) {
                return Err(mlua::Error::runtime("native API tables cannot mix object and array keys"));
            }
            if array.is_empty() && !explicit_array {
                Json::Object(object)
            } else {
                if array.len() != array.keys().next_back().copied().unwrap_or(0) {
                    return Err(mlua::Error::runtime("native API arrays must start at 1 and have no gaps; use omsi.null for an empty element"));
                }
                Json::Array(array.into_values().collect())
            }
        }
        _ => return Err(mlua::Error::runtime("native API arguments must be values or tables; functions, threads and userdata are not supported")),
    })
}

fn from_json(
    lua: &Lua,
    value: Json,
    depth: usize,
    budget: &mut Budget,
    null: &Table,
    array_meta: &Table,
) -> mlua::Result<Value> {
    budget.enter(depth)?;
    Ok(match value {
        // A stable sentinel preserves missing array elements and explicitly unavailable
        // object fields instead of silently deleting them as a Lua nil assignment would.
        Json::Null => Value::Table(null.clone()),
        Json::Bool(b) => Value::Boolean(b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(u) = n.as_u64() {
                return Err(mlua::Error::runtime(format!("native API integer {u} exceeds Lua's signed integer range; engine handles must be strings")));
            } else {
                let f = n
                    .as_f64()
                    .ok_or_else(|| mlua::Error::runtime("invalid native API number"))?;
                // JSON floating values are intentionally retained as floats, including
                // fractional simulator time and positions. Identity values use strings.
                Value::Number(f)
            }
        }
        Json::String(s) => {
            budget.text(s.len())?;
            Value::String(lua.create_string(&s)?)
        }
        Json::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            table.set_metatable(Some(array_meta.clone()));
            for (i, value) in values.into_iter().enumerate() {
                table.raw_set(
                    i + 1,
                    from_json(lua, value, depth + 1, budget, null, array_meta)?,
                )?;
            }
            Value::Table(table)
        }
        Json::Object(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (name, value) in values {
                budget.text(name.len())?;
                table.raw_set(
                    name,
                    from_json(lua, value, depth + 1, budget, null, array_meta)?,
                )?;
            }
            Value::Table(table)
        }
    })
}

pub(crate) fn install(
    lua: &Lua,
    omsi: &Table,
    call: impl Fn(&str, Json, &[u8]) -> Result<Json, String> + 'static,
) -> mlua::Result<()> {
    let null = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__metatable", false)?;
    meta.set(
        "__newindex",
        lua.create_function(|_, _: (Value, Value, Value)| -> mlua::Result<()> {
            Err(mlua::Error::runtime(
                "omsi.null is a sentinel and cannot be modified",
            ))
        })?,
    )?;
    null.set_metatable(Some(meta));
    omsi.set("null", null.clone())?;
    let array_meta = lua.create_table()?;
    array_meta.raw_set("__metatable", false)?;
    let marker = array_meta.clone();
    let null_marker = null.clone();
    omsi.set(
        "array",
        lua.create_function(move |lua, input: Option<Table>| {
            let table = match input {
                Some(table) => table,
                None => lua.create_table()?,
            };
            if table.to_pointer() == null_marker.to_pointer() {
                return Err(mlua::Error::runtime(
                    "omsi.null cannot be converted to an array",
                ));
            }
            if table
                .metatable()
                .is_some_and(|meta| meta.to_pointer() != marker.to_pointer())
            {
                return Err(mlua::Error::runtime(
                    "omsi.array requires a plain table or an existing API array",
                ));
            }
            let mut count = 0usize;
            let mut last = 0usize;
            for pair in table.pairs::<Value, Value>() {
                let (key, _) = pair?;
                let Value::Integer(index) = key else {
                    return Err(mlua::Error::runtime(
                        "omsi.array keys must be consecutive positive integers",
                    ));
                };
                if index <= 0 || index > MAX_NODES as i64 {
                    return Err(mlua::Error::runtime(
                        "omsi.array index is outside the supported range",
                    ));
                }
                count += 1;
                last = last.max(index as usize);
            }
            if count != last {
                return Err(mlua::Error::runtime(
                    "omsi.array cannot contain gaps; use omsi.null for an empty element",
                ));
            }
            // No input table is changed until all its keys have been validated.
            table.set_metatable(Some(marker.clone()));
            Ok(table)
        })?,
    )?;
    omsi.set("api", lua.create_function(move |lua, (operation, args, binary): (String, Option<Value>, Option<mlua::String>)| {
        if operation.is_empty() || operation.len() > 128 {
            return Err(mlua::Error::runtime("native API operation must contain 1..128 bytes"));
        }
        let arguments = match args {
            None | Some(Value::Nil) => Json::Object(Map::new()),
            Some(value) => to_json(value, 0, &mut Budget::default(), &null, &array_meta)?,
        };
        if !arguments.is_object() {
            return Err(mlua::Error::runtime("native API arguments must be a table with named fields"));
        }
        let bytes = binary.as_ref().map(|s| s.as_bytes());
        let bytes = bytes.as_deref().unwrap_or_default();
        if bytes.len() > MAX_BINARY {
            return Err(mlua::Error::runtime("native API binary payload exceeds 16 MiB"));
        }
        let reply = call(&operation, arguments, bytes).map_err(mlua::Error::runtime)?;
        from_json(lua, reply, 0, &mut Budget::default(), &null, &array_meta).map_err(|error|
            mlua::Error::runtime(format!("native API operation returned successfully, but its reply could not be delivered; state may already have changed: {error}")))
    })?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn fixture(call: impl Fn(&str, Json, &[u8]) -> Result<Json, String> + 'static) -> Lua {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        install(&lua, &table, call).unwrap();
        lua.globals().set("omsi", table).unwrap();
        lua
    }

    #[test]
    fn nested_data_binary_and_null_round_trip() {
        let lua = fixture(|operation, arguments, bytes| {
            assert_eq!(operation, "test.echo");
            assert_eq!(bytes, &[0, 255, 127, 0]);
            Ok(arguments)
        });
        lua.load(
            r#"
            local value = omsi.api("test.echo", {
              name = "Příští zastávka", time = 12.375, flag = true,
              entries = { 1, omsi.null, { id = "vehicle:12345678901234567890" } },
              unavailable = omsi.null, signed = -9223372036854775807
            }, string.char(0, 255, 127, 0))
            assert(value.name == "Příští zastávka" and value.time == 12.375)
            assert(value.flag and value.unavailable == omsi.null)
            assert(#value.entries == 3 and value.entries[2] == omsi.null)
            assert(value.entries[3].id == "vehicle:12345678901234567890")
            assert(value.signed == -9223372036854775807)
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn explicit_and_returned_empty_arrays_preserve_their_native_type() {
        let lua = fixture(|operation, arguments, _| {
            assert_eq!(operation, "test.arrays");
            assert_eq!(arguments["empty"], serde_json::json!([]));
            assert_eq!(arguments["plain"], serde_json::json!({}));
            Ok(serde_json::json!({"empty":[],"nested":[[],null]}))
        });
        lua.load(
            r#"
            local result=omsi.api("test.arrays",{empty=omsi.array{},plain={}})
            assert(#result.empty==0 and #result.nested==2 and result.nested[2]==omsi.null)
            omsi.api("test.arrays",{empty=result.empty,plain={}})
            omsi.api("test.arrays",{empty=result.nested[1],plain={}})
            omsi.api("test.arrays",{empty=omsi.array(),plain={}})
            assert(getmetatable(result.empty)==false)
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn array_markers_reject_bad_keys_and_preserve_null_without_metamethod_calls() {
        let lua = fixture(|_, _, _| panic!("invalid arrays must not reach the game"));
        lua.load(
            r#"
            assert(not pcall(omsi.array,omsi.null))
            assert(not pcall(omsi.array,{[2]=1}))
            assert(not pcall(omsi.array,{1,named=true}))
            assert(not pcall(function() omsi.null.changed=true end))
            local touched=0
            local t=setmetatable({}, {__pairs=function() touched=touched+1 end,
                __index=function() touched=touched+1 end,__eq=function() touched=touched+1 end})
            assert(not pcall(omsi.array,t))
            assert(touched==0)
            local array=omsi.array{1}
            array.named=true
            assert(not pcall(omsi.api,"test.write",{value=array}))
            array.named=nil;array[1]=nil;array[2]=1
            assert(not pcall(omsi.api,"test.write",{value=array}))
            assert(not pcall(setmetatable,array,{}))
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn reply_limit_does_not_disguise_a_completed_operation() {
        let count = Rc::new(Cell::new(0));
        let calls = count.clone();
        let lua = fixture(move |_, _, _| {
            calls.set(calls.get() + 1);
            Ok(Json::String("x".repeat(MAX_TEXT + 1)))
        });
        let error = lua
            .load("return omsi.api('test.large_reply')")
            .eval::<Value>()
            .unwrap_err()
            .to_string();
        assert_eq!(count.get(), 1);
        assert!(error.contains("state may already have changed"), "{error}");
    }

    #[test]
    fn bad_arguments_are_rejected_before_game_mutation() {
        let count = Rc::new(Cell::new(0));
        let calls = count.clone();
        let lua = fixture(move |_, _, _| {
            calls.set(calls.get() + 1);
            Ok(Json::Bool(true))
        });
        lua.load(
            r#"
            local cycle = {}; cycle.self = cycle
            for _, value in ipairs({
              { bad = 0/0 }, { bad = math.huge }, { bad = function() end },
              { bad = { [1] = 1, [3] = 3 } }, { bad = { 1, named = true } }, cycle
            }) do
              assert(not pcall(omsi.api, "test.write", value))
            end
            assert(not pcall(omsi.api, "test.write", { text = string.rep("x", 2*1024*1024+1) }))
            assert(not pcall(omsi.api, "test.write", {}, string.rep("x", 16*1024*1024+1)))
        "#,
        )
        .exec()
        .unwrap();
        assert_eq!(count.get(), 0);
    }

    #[test]
    fn unsupported_operations_preserve_the_engine_error() {
        let lua = fixture(|_, _, _| Err("unsupported operation: humans.teleport".into()));
        lua.load(r#"
            local ok, err = pcall(omsi.api, "humans.teleport", {})
            assert(not ok and string.find(tostring(err), "unsupported operation: humans.teleport", 1, true))
        "#).exec().unwrap();
    }
}
