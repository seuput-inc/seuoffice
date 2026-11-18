//! Snapshot-backed workbook pivot, slicer and connection readers.
mod connections;
mod pivot;
mod slicers;
use std::collections::HashSet;
use shell_contract::IpcValue;
use shell_ipc_d1::sheets::engine::{EngineResult,engine_error};
use super::{workbook_rpc::Rpc,workbook_schema,workbook_xml};

struct Package<'a>{rpc:&'a Rpc,path:&'a str,names:HashSet<String>,paths:Vec<String>}
impl<'a> Package<'a>{
 fn open(rpc:&'a Rpc,path:&'a str)->EngineResult<Self>{let paths=rpc.names(path)?;let names=paths.iter().cloned().collect();Ok(Self{rpc,path,names,paths})}
 fn get(&self,name:&str)->EngineResult<Option<String>>{if self.names.contains(name){self.rpc.read_text(self.path,name).map(Some)}else{Ok(None)}}
 fn relations(&self,path:&str)->EngineResult<Vec<workbook_xml::Relationship>>{match self.get(&workbook_xml::rels_path(path))?{Some(s)=>workbook_xml::relationships(&s),None=>Ok(Vec::new())}}
}
fn ipc(v:&serde_json::Value)->IpcValue{shell_ipc_d1::value::from_json(&jscompat::json_value_from_serde(v))}
fn validate(name:&str,v:serde_json::Value)->EngineResult<IpcValue>{workbook_schema::parse(name,&ipc(&v))}
fn resolve(part:&str,target:&str)->Option<String>{super::workbook_save_planner::resolve_rel_target(part,target).ok()}
pub(super) fn pivot(rpc:&Rpc,snapshot:&str,pivot_path:&str,cache_path:&str)->EngineResult<IpcValue>{let p=rpc.read_text(snapshot,pivot_path)?;let c=rpc.read_text(snapshot,cache_path)?;validate("pivot",pivot::parse(&p,&c)?)}
pub(super) fn slicers(rpc:&Rpc,snapshot:&str)->EngineResult<IpcValue>{let pkg=Package::open(rpc,snapshot)?;validate("slicers",slicers::read(&pkg)?)}
pub(super) fn connections(rpc:&Rpc,snapshot:&str)->EngineResult<IpcValue>{let pkg=Package::open(rpc,snapshot)?;validate("connections",serde_json::json!({"connections":connections::refreshable(&pkg)?}))}
pub(super) fn queries(rpc:&Rpc,snapshot:&str)->EngineResult<IpcValue>{let pkg=Package::open(rpc,snapshot)?;validate("queries",serde_json::Value::Array(connections::list(&pkg)?))}
pub(super) fn refresh_connection(rpc:&Rpc,snapshot:&str,id:&str)->EngineResult<IpcValue>{
 let pkg=Package::open(rpc,snapshot)?;let xml=pkg.get("xl/connections.xml")?.ok_or_else(||engine_error("This workbook has no refreshable web query with that id."))?;
 let source=connections::parse(&xml)?.into_iter().find(|c|c.id==id&&c.query.get("kind").and_then(serde_json::Value::as_str)==Some("web")).and_then(|c|c.query.get("source").and_then(serde_json::Value::as_str).map(str::to_owned)).filter(|s|!s.is_empty()).ok_or_else(||engine_error("This workbook has no refreshable web query with that id."))?;
 let mut resolver=crate::host::net::ai::fetch::guarded_resolver();let mut fetcher=crate::host::net::ai::fetch::guarded_fetcher_with_cap(16*1024*1024);
 let deadline=Deadline(std::time::Instant::now()+std::time::Duration::from_secs(30));
 let table=main_core::packages::electron_utils::remote_table::fetch_remote_table(resolver.as_mut(),fetcher.as_mut(),&deadline,&source).ok_or_else(||engine_error("Could not read a table from that address."))?;
 validate("refresh",serde_json::json!({"headers":table.headers,"rows":table.rows}))
}
struct Deadline(std::time::Instant);
impl main_core::packages::electron_utils::safe_remote_url::AbortSignal for Deadline{fn aborted(&self)->bool{std::time::Instant::now()>=self.0}}
