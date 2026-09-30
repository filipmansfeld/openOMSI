//! Live vehicle emitters and particles. Definitions remain native script bindings.
use crate::App;
use omsi_model::{ParticleSystemDef,PsRange,PsValue};
use omsi_sim::particles::{Particle,ParticleSet};
use serde_json::{json,Value};

fn keys(args:&Value,allowed:&[&str])->Result<(),String> {
    for key in args.as_object().ok_or("arguments must be an object")?.keys() {
        if !["id","vehicle_id","generation","session_id","section","particle_generation"].contains(&key.as_str()) && !allowed.contains(&key.as_str()) {
            return Err(format!("unsupported particle argument: {key}"));
        }
    }
    Ok(())
}
fn index(args:&Value,key:&str,default:Option<usize>)->Result<usize,String> {
    match args.get(key){Some(v)=>v.as_u64().and_then(|n|usize::try_from(n).ok()).ok_or_else(||format!("{key} must be an integer")),
        None=>default.ok_or_else(||format!("{key} is required"))}
}
fn number(v:&Value,key:&str,min:f64,max:f64)->Result<f64,String> {
    v.as_f64().filter(|n|n.is_finite() && (min..=max).contains(n)).ok_or_else(||format!("{key} must be between {min} and {max}"))
}
fn vector(v:&Value,key:&str,min:f64,max:f64)->Result<[f64;3],String> {
    let v=v.as_array().filter(|v|v.len()==3).ok_or_else(||format!("{key} requires exactly three numbers"))?;
    Ok([number(&v[0],key,min,max)?,number(&v[1],key,min,max)?,number(&v[2],key,min,max)?])
}
fn binding(v:&Value,key:&str,min:f32,max:f32,has_var:&dyn Fn(&str)->bool)->Result<PsValue,String> {
    if let Some(obj)=v.as_object() {
        if obj.len()!=1 || !obj.contains_key("variable"){return Err(format!("{key} binding requires only variable"));}
        let name=v["variable"].as_str().filter(|n|!n.is_empty() && n.len()<=256).ok_or("invalid particle variable name")?;
        if !has_var(name){return Err(format!("unknown particle variable: {name}"));}
        Ok(PsValue::Var(name.into()))
    } else {Ok(PsValue::Const(number(v,key,min as f64,max as f64)? as f32))}
}
fn range(v:&Value,key:&str,min:f32,max:f32,spread:f32,has_var:&dyn Fn(&str)->bool)->Result<PsRange,String> {
    if v.is_number() || v.get("variable").is_some(){return Ok((binding(v,key,min,max,has_var)?,PsValue::Const(0.0)));}
    let obj=v.as_object().ok_or_else(||format!("{key} requires a number/binding or base+variation record"))?;
    if obj.len()!=2 || !obj.contains_key("base") || !obj.contains_key("variation"){return Err(format!("{key} requires only base and variation"));}
    Ok((binding(&v["base"],key,min,max,has_var)?,binding(&v["variation"],key,0.0,spread,has_var)?))
}
fn binding_json(v:&PsValue)->Value {match v {PsValue::Const(n)=>json!(n),PsValue::Var(name)=>json!({"variable":name})}}
fn range_json(v:&PsRange)->Value {json!({"base":binding_json(&v.0),"variation":binding_json(&v.1)})}
fn definition(d:&ParticleSystemDef)->Value {
    json!({"position_local":d.pos,"direction_local":d.dir,"velocity_metres_per_second":range_json(&d.velocity),
        "velocity_all_round":d.velocity_all_round,"frequency_per_second":binding_json(&d.freq.0),
        "lifetime_seconds":range_json(&d.life),"brake_factor":range_json(&d.brake),"gravity_factor":range_json(&d.gravity),
        "size_start_metres":range_json(&d.size_start),"size_growth_metres_per_second":range_json(&d.size_grow),
        "alpha_initial":range_json(&d.alpha_initial),"alpha_final":range_json(&d.alpha_final),
        "color":d.rgb.iter().map(range_json).collect::<Vec<_>>(),"calculation_distance_metres":d.calc_dist,"emissive":d.emissive,
        "authored_metadata":{"bitmap":d.bitmap,"attachment":d.attach,"burst":d.burst.as_ref().map(range_json),"frequency_variation":binding_json(&d.freq.1)}})
}

fn emitter_patch(def:&ParticleSystemDef,max_particles:usize,values:&Value,has_var:&dyn Fn(&str)->bool)->Result<(ParticleSystemDef,usize),String> {
    let mut def=def.clone();let mut max_particles=max_particles;
    let values=values.as_object().filter(|v|!v.is_empty()).ok_or("values must be a nonempty object")?;
    for (key,value) in values {
        match key.as_str() {
            "position_local"=>def.pos=vector(value,key,-1e6,1e6)?.map(|v|v as f32),
            "direction_local"=>{
                let direction=vector(value,key,-1e6,1e6)?.map(|v|v as f32);
                if glam::Vec3::from_array(direction).length_squared()<1e-12{return Err("emission direction must not be zero".into());}
                def.dir=direction;
            }
            "velocity_metres_per_second"=>def.velocity=range(value,key,-1e4,1e4,1e4,has_var)?,
            "frequency_per_second"=>def.freq.0=binding(value,key,0.0,1e6,has_var)?,
            "lifetime_seconds"=>def.life=range(value,key,0.05,86400.0,86400.0,has_var)?,
            "brake_factor"=>def.brake=range(value,key,0.0,1.5,1.5,has_var)?,
            "gravity_factor"=>def.gravity=range(value,key,-1000.0,1000.0,1000.0,has_var)?,
            "size_start_metres"=>def.size_start=range(value,key,0.0,1e4,1e4,has_var)?,
            "size_growth_metres_per_second"=>def.size_grow=range(value,key,-1000.0,1000.0,1000.0,has_var)?,
            "alpha_initial"=>def.alpha_initial=range(value,key,0.0,1.0,1.0,has_var)?,
            "alpha_final"=>def.alpha_final=range(value,key,0.0,1.0,1.0,has_var)?,
            "color"=>{
                let rgb=value.as_array().filter(|v|v.len()==3).ok_or("color must contain three native ranges")?;
                def.rgb=[range(&rgb[0],key,0.0,1.0,1.0,has_var)?,range(&rgb[1],key,0.0,1.0,1.0,has_var)?,range(&rgb[2],key,0.0,1.0,1.0,has_var)?];
            }
            "calculation_distance_metres"=>def.calc_dist=number(value,key,50.0,1e6)? as f32,
            "emissive"=>def.emissive=value.as_bool().ok_or("emissive must be boolean")?,
            "velocity_all_round"=>def.velocity_all_round=value.as_bool().ok_or("velocity_all_round must be boolean")?,
            "max_particles"=>max_particles=value.as_u64().filter(|n|*n<=4096).ok_or("max_particles must be an integer in 0..4096")? as usize,
            _=>return Err(format!("unsupported emitter property: {key}")),
        }
    }
    Ok((def,max_particles))
}

fn particle_record(emitter:usize,p:&Particle)->Value {
    json!({"particle_id":format!("particle:{}",p.api_id),"emitter":emitter,"world_position":p.pos.to_array(),"velocity_world":p.vel.to_array(),
        "age_seconds":p.age,"lifetime_seconds":p.life,"size_start_metres":p.size0,"size_growth_metres_per_second":p.grow,
        "size_metres":p.size(),"alpha_initial":p.alpha0,"alpha_final":p.alpha1,"alpha":p.alpha(),"color":p.color,
        "brake_factor":p.brake,"gravity_factor":p.gravity})
}
fn particle_id(value:&Value)->Result<u64,String> {
    value.as_str().and_then(|v|v.strip_prefix("particle:")).and_then(|v|v.parse().ok()).ok_or_else(||"invalid particle_id".into())
}
fn particle_patch(current:&Particle,values:&Value)->Result<Particle,String> {
    let values=values.as_object().filter(|v|!v.is_empty()).ok_or("values must be a nonempty object")?;
    let mut p=current.clone();
    for (key,value) in values {
        match key.as_str() {
            "world_position"=>{
                let v=vector(value,key,-1e9,1e9)?;
                if v[2].abs()>1e6{return Err("particle altitude exceeds the native world range".into());}
                p.pos=glam::DVec3::from_array(v);
            }
            "velocity_world"=>p.vel=glam::Vec3::from_array(vector(value,key,-1e4,1e4)?.map(|v|v as f32)),
            "age_seconds"=>p.age=number(value,key,0.0,86400.0)? as f32,
            "lifetime_seconds"=>p.life=number(value,key,0.001,86400.0)? as f32,
            "size_start_metres"=>p.size0=number(value,key,0.0,1e4)? as f32,
            "size_growth_metres_per_second"=>p.grow=number(value,key,-1000.0,1000.0)? as f32,
            "alpha_initial"=>p.alpha0=number(value,key,0.0,1.0)? as f32,
            "alpha_final"=>p.alpha1=number(value,key,0.0,1.0)? as f32,
            "color"=>p.color=vector(value,key,0.0,1.0)?.map(|v|v as f32),
            "brake_factor"=>p.brake=number(value,key,0.0,1.5)? as f32,
            "gravity_factor"=>p.gravity=number(value,key,-1000.0,1000.0)? as f32,
            _=>return Err(format!("unsupported particle property: {key}")),
        }
    }
    if p.age>p.life{return Err("particle age exceeds its lifetime".into());}
    Ok(p)
}

fn run(set:&mut ParticleSet,authored:&[ParticleSystemDef],operation:&str,args:&Value,has_var:&dyn Fn(&str)->bool)->Result<Value,String> {
    let writing=!matches!(operation,"particles.list"|"particles.get"|"particles.emitters.list"|"particles.emitters.get");
    if let Some(g)=args.get("particle_generation") {
        if g.as_str().and_then(|s|s.parse::<u64>().ok())!=Some(set.api_id){return Err("stale particle generation".into());}
    } else if writing {return Err("particle_generation from emitter/particle enumeration is required".into());}
    let epoch=set.api_id.to_string();
    if operation=="particles.emitters.list" {
        let offset=index(args,"offset",Some(0))?;let limit=index(args,"limit",Some(64))?;
        if !(1..=128).contains(&limit) || offset>set.emitters.len(){return Err("invalid emitter page (limit 1..128)".into());}
        let end=offset.saturating_add(limit).min(set.emitters.len());
        return Ok(json!({"particle_generation":epoch,"total":set.emitters.len(),"next_offset":if end<set.emitters.len(){Some(end)}else{None},
            "items":set.emitters[offset..end].iter().enumerate().map(|(i,e)|json!({"emitter":offset+i,"live_particles":e.particles.len(),"max_particles":e.max_particles})).collect::<Vec<_>>()}));
    }
    if operation.starts_with("particles.emitters.") {
        let index=index(args,"emitter",None)?;
        let current=set.emitters.get(index).ok_or("emitter is unavailable")?;
        if operation=="particles.emitters.get" {return Ok(json!({"emitter":index,"particle_generation":epoch,
            "live_particles":current.particles.len(),"max_particles":current.max_particles,"definition":definition(&current.def)}));}
        let (definition,maximum)=if operation=="particles.emitters.reset" {
            (authored.get(index).ok_or("authored emitter definition is unavailable")?.clone(),omsi_sim::particles::MAX_PER_EMITTER)
        } else {emitter_patch(&current.def,current.max_particles,&args["values"],has_var)?};
        let other_maximum:usize=set.emitters.iter().enumerate().filter(|(i,_)|*i!=index).map(|(_,e)|e.max_particles).sum();
        if other_maximum.saturating_add(maximum)>65536{return Err("particle section capacity exceeds 65536".into());}
        let emitter=&mut set.emitters[index];
        emitter.def=definition;emitter.max_particles=maximum;emitter.particles.truncate(maximum);
        return Ok(json!({"emitter":index,"particle_generation":epoch,"queued":true}));
    }
    if operation=="particles.clear" {
        let range=if args.get("emitter").is_some() {
            let i=index(args,"emitter",None)?;if i>=set.emitters.len(){return Err("emitter is unavailable".into());}i..i+1
        }else{0..set.emitters.len()};
        let mut removed=0;
        for emitter in &mut set.emitters[range]{removed+=emitter.particles.len();emitter.particles.clear();}
        return Ok(json!({"removed":removed,"particle_generation":epoch}));
    }
    if operation=="particles.list" {
        let limit=index(args,"limit",Some(100))?;
        if !(1..=256).contains(&limit){return Err("particle list limit must be in 1..256".into());}
        let after=args.get("after_id").map(particle_id).transpose()?.unwrap_or(0);
        let emitter=args.get("emitter").map(|_|index(args,"emitter",None)).transpose()?;
        if emitter.is_some_and(|i|i>=set.emitters.len()){return Err("emitter is unavailable".into());}
        let mut all:Vec<_>=set.emitters.iter().enumerate().filter(|(i,_)|emitter.is_none_or(|e|e==*i))
            .flat_map(|(i,e)|e.particles.iter().map(move|p|(i,p))).filter(|(_,p)|p.api_id>after).collect();
        all.sort_unstable_by_key(|(_,p)|p.api_id);
        let has_more=all.len()>limit;all.truncate(limit);
        return Ok(json!({"particle_generation":epoch,"items":all.iter().map(|(i,p)|particle_record(*i,p)).collect::<Vec<_>>(),
            "next_after_id":if has_more{all.last().map(|(_,p)|format!("particle:{}",p.api_id))}else{None}}));
    }
    let id=particle_id(&args["particle_id"])?;
    let (emitter,particle)=set.emitters.iter_mut().enumerate().find_map(|(i,e)|e.particles.iter_mut().find(|p|p.api_id==id).map(|p|(i,p)))
        .ok_or("particle has expired or is not owned by this section")?;
    if operation=="particles.set"{*particle=particle_patch(particle,&args["values"])?;}
    let mut result=particle_record(emitter,particle);result["particle_generation"]=json!(epoch);Ok(result)
}

pub(crate) fn execute(app:&mut App,id:u64,operation:&str,args:&Value)->Option<Result<Value,String>> {
    if !operation.starts_with("particles."){return None;}
    Some((||{
        let allowed:&[&str]=match operation {
            "particles.emitters.list"=>&["offset","limit"],"particles.emitters.get"|"particles.emitters.reset"=>&["emitter"],
            "particles.emitters.set"=>&["emitter","values"],"particles.list"=>&["emitter","after_id","limit"],
            "particles.get"=>&["particle_id"],"particles.set"=>&["particle_id","values"],"particles.clear"=>&["emitter"],
            _=>return Err(format!("unsupported particle operation: {operation}")),
        };
        keys(args,allowed)?;
        let writing=!matches!(operation,"particles.list"|"particles.get"|"particles.emitters.list"|"particles.emitters.get");
        if writing && !args.get("session_id").is_some_and(Value::is_string){return Err("session_id is required for particle writes".into());}
        let section=index(args,"section",Some(0))?;
        let vehicle=&mut app.player.iter_mut().chain(app.placed.iter_mut()).find(|p|p.uid==id).ok_or("vehicle is no longer loaded")?.vehicle;
        // The shared leading script owns trailer bindings as well. Resolve names
        // before borrowing any particle set mutably.
        let names:std::collections::BTreeSet<_>=if operation=="particles.emitters.set" {
            vehicle.ty.program.var_names.iter().map(|n|n.to_ascii_lowercase())
                .chain((vehicle.ty.program.var_names.len()..vehicle.state.vars.len())
                    .filter_map(|i|vehicle.var_name(i).map(str::to_ascii_lowercase))).collect()
        }else{Default::default()};
        let has_var=|name:&str|names.contains(&name.to_ascii_lowercase());
        if section==0 {
            let authored=if operation=="particles.emitters.reset"{vehicle.ty.model.particle_systems()}else{Vec::new()};
            run(&mut vehicle.particles,&authored,operation,args,&has_var)
        } else {
            let trailer=vehicle.trailers.get_mut(section-1).ok_or("vehicle section is unavailable")?;
            let authored=if operation=="particles.emitters.reset"{trailer.ty.model.particle_systems()}else{Vec::new()};
            run(&mut trailer.particles,&authored,operation,args,&has_var)
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition_fixture()->ParticleSystemDef {
        ParticleSystemDef{dir:[0.0,0.0,1.0],life:(PsValue::Const(10.0),PsValue::Const(0.0)),
            brake:(PsValue::Const(1.0),PsValue::Const(0.0)),..Default::default()}
    }
    #[test]
    fn emitter_changes_produce_real_particles_and_live_writes_change_their_motion() {
        let authored=vec![definition_fixture()];let mut set=ParticleSet::new(authored.clone(),7);
        let generation=set.api_id.to_string();
        run(&mut set,&authored,"particles.emitters.set",&json!({"particle_generation":generation,"emitter":0,
            "values":{"frequency_per_second":4,"max_particles":2}}),&|_|false).unwrap();
        set.update(0.25,glam::DVec3::ZERO,glam::Mat4::IDENTITY,&|_|0.0);
        assert_eq!(set.emitters[0].particles.len(),1);
        let id=format!("particle:{}",set.emitters[0].particles[0].api_id);
        run(&mut set,&authored,"particles.set",&json!({"particle_generation":generation,"particle_id":id,
            "values":{"velocity_world":[0,0,2],"gravity_factor":0}}),&|_|false).unwrap();
        set.update(0.5,glam::DVec3::ZERO,glam::Mat4::IDENTITY,&|_|0.0);
        assert_eq!(set.emitters[0].particles.len(),2);
        assert_eq!(set.emitters[0].particles[0].pos.z,1.0);
        run(&mut set,&authored,"particles.clear",&json!({"particle_generation":generation}),&|_|false).unwrap();
        set.update(0.5,glam::DVec3::ZERO,glam::Mat4::IDENTITY,&|_|0.0);
        assert!(run(&mut set,&authored,"particles.get",&json!({"particle_id":id}),&|_|false).is_err());
    }
    #[test]
    fn invalid_emitter_batch_and_stale_generation_leave_simulation_unchanged() {
        let authored=vec![definition_fixture()];let mut set=ParticleSet::new(authored.clone(),7);
        let epoch=set.api_id.to_string();
        for values in [json!({"frequency_per_second":4,"unknown":1}),json!({"frequency_per_second":{"variable":"missing"}}),
            json!({"frequency_per_second":4,"color":[1,2,3]}),json!({"max_particles":4097})] {
            assert!(run(&mut set,&authored,"particles.emitters.set",&json!({"particle_generation":epoch,"emitter":0,"values":values}),&|_|false).is_err());
            assert_eq!(set.emitters[0].def,authored[0]);
        }
        assert!(run(&mut set,&authored,"particles.clear",&json!({"particle_generation":"0"}),&|_|false).is_err());
    }
}
