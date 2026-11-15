use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use super::FontRenderer;

/// The evaluator must be bound to the ingress native document token and main
/// execution context. Current-context evaluation after navigation is unsafe.
pub(super) struct BoundRenderer<E> { evaluate: E }
impl<E> BoundRenderer<E> {
    pub(super) fn new(evaluate: E) -> Self { Self { evaluate } }
}
impl<E> FontRenderer for BoundRenderer<E> where E: FnMut(&str) -> Result<Value, String> + Send {
    fn register(&mut self, family: &str, bold: bool, italic: bool, weight_range: Option<&str>, sfnt: &[u8]) -> Result<(), String> {
        let weight = weight_range.map(str::to_owned).unwrap_or_else(|| if bold { "700".into() } else { "400".into() });
        let payload = json!({"family":family,"weight":weight,"style":if italic { "italic" } else { "normal" },"bytes":STANDARD.encode(sfnt)});
        let script = format!(r#"(async()=>{{const p={payload};let have=false;document.fonts.forEach(x=>{{if(x.family===p.family&&x.weight===p.weight&&x.style===p.style)have=true}});if(have)return true;const bin=atob(p.bytes);const bytes=new Uint8Array(bin.length);for(let i=0;i<bin.length;i++)bytes[i]=bin.charCodeAt(i);const face=new FontFace(p.family,bytes.buffer,{{weight:p.weight,style:p.style}});document.fonts.add(face);await face.load();document.fonts.dispatchEvent(new Event('loadingdone'));return true}})()"#);
        if (self.evaluate)(&script)? != Value::Bool(true) { return Err("renderer did not register font".into()); }
        Ok(())
    }
    fn measure_100px(&mut self, text: &str, family: &str, bold: bool, italic: bool) -> Result<f64, String> {
        let payload = json!({"text":text,"family":family,"bold":bold,"italic":italic});
        let script = format!(r#"(()=>{{const p={payload};const ctx=document.createElement('canvas').getContext('2d');if(!ctx)throw new Error('Canvas unavailable');const stack=typeof window.__seuofficeDisplayFontFamily==='function'?window.__seuofficeDisplayFontFamily:(f)=>'"'+f+'", \'PingFang SC\', \'Microsoft YaHei\', \'Yu Gothic\', \'Malgun Gothic\', sans-serif';ctx.font=(p.italic?'italic ':'')+(p.bold?'bold ':'')+'100px '+stack(p.family);return ctx.measureText(p.text).width}})()"#);
        (self.evaluate)(&script)?.as_f64().filter(|width| width.is_finite()).ok_or_else(|| "renderer returned invalid font width".into())
    }
}
