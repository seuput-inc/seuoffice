//! The byte-level encryption dictionary selection used by pdf-crypt.ts.
use jscompat::TryJsRegex;

fn space(b:u8)->bool { jscompat::is_js_space(char::from(b)) }
fn word(b:u8)->bool { b.is_ascii_alphanumeric()||b==b'_' }
fn digits(data:&[u8],at:&mut usize)->Option<f64>{let start=*at;while data.get(*at).is_some_and(u8::is_ascii_digit){*at+=1;}if *at==start{return None}std::str::from_utf8(&data[start..*at]).ok()?.parse::<f64>().ok()}
fn spaces(data:&[u8],at:&mut usize)->bool{let start=*at;while data.get(*at).copied().is_some_and(space){*at+=1;}*at>start}
fn seek(data:&[u8],needle:&[u8])->Option<usize>{data.windows(needle.len()).position(|s|s==needle)}
pub(super) fn reference(data:&[u8])->Option<(f64,f64)>{
    for at in 0..data.len(){if !data[at..].starts_with(b"/Encrypt"){continue}let mut i=at+8;if !spaces(data,&mut i){continue}let Some(n)=digits(data,&mut i) else{continue};if !spaces(data,&mut i){continue}let Some(g)=digits(data,&mut i) else{continue};if !spaces(data,&mut i){continue}if data.get(i)==Some(&b'R'){return Some((n,g))}}
    None
}
struct Header{start:usize,body:usize,num:f64,generation:f64}
fn next_header(data:&[u8],mut pos:usize)->Option<Header>{while pos<data.len(){let start=pos;let mut at=if space(data[pos]){pos+1}else if pos==0{0}else{pos+=1;continue};let Some(num)=digits(data,&mut at)else{pos+=1;continue};if !spaces(data,&mut at){pos+=1;continue}let Some(generation)=digits(data,&mut at)else{pos+=1;continue};if !spaces(data,&mut at)||!data[at..].starts_with(b"obj")||data.get(at+3).is_some_and(|b|word(*b)){pos+=1;continue}return Some(Header{start,body:at+3,num,generation})}None}
/// Returns the raw dictionary selected by the first /Encrypt reference, or None.
pub(super) fn encrypt_dictionary(data:&[u8])->Option<&[u8]>{
    let (num,generation)=reference(data)?;let mut pos=0;
    let header=loop{let header=next_header(data,pos)?;pos=header.body;if header.num==num&&header.generation==generation{break header}};
    let limit=next_header(data,header.body).map_or(data.len(),|h|h.start);let body=&data[header.body..limit];
    let stream=body.windows(6).enumerate().find_map(|(at,window)|{
        if window!=b"stream"||at>0&&word(body[at-1]){return None}
        matches!(body.get(at+6),Some(b'\r'|b'\n')).then_some(at)
    });
    let end=stream.or_else(||seek(body,b"endobj")).unwrap_or(body.len());Some(&body[..end])
}
fn latin1(bytes:&[u8])->String{bytes.iter().map(|&b|char::from(b)).collect()}
fn hex(data:&[u8],at:&mut usize)->Option<Vec<u8>> {
    if data.get(*at)!=Some(&b'<'){return None}*at+=1;let mut out=Vec::new();let mut high=None;
    while let Some(&b)=data.get(*at) { if b==b'>'{*at+=1;if let Some(n)=high{out.push(n<<4)}return Some(out)} if space(b){*at+=1;continue}let value=(b as char).to_digit(16)? as u8;if let Some(n)=high.take(){out.push((n<<4)|value)}else{high=Some(value)}*at+=1; } None
}
/// First raw trailer ID pair, as carried by the original re-protection path.
pub(super) fn file_id(data:&[u8])->Option<[Vec<u8>;2]> {
    for start in 0..data.len() {if !data[start..].starts_with(b"/ID"){continue}let mut at=start+3;while data.get(at).copied().is_some_and(space){at+=1}if data.get(at)!=Some(&b'['){continue}at+=1;while data.get(at).copied().is_some_and(space){at+=1}let Some(first)=hex(data,&mut at) else{continue};while data.get(at).copied().is_some_and(space){at+=1}let Some(second)=hex(data,&mut at) else{continue};while data.get(at).copied().is_some_and(space){at+=1}if data.get(at)==Some(&b']'){return Some([first,second])}}
    None
}
/// The first raw ID value used to derive a legacy file key.
pub(super) fn first_id(data:&[u8])->Vec<u8> {
    for start in 0..data.len() {
        if !data[start..].starts_with(b"/ID"){continue}
        let mut at=start+3;while data.get(at).copied().is_some_and(space){at+=1}
        if data.get(at)!=Some(&b'['){continue}at+=1;
        while data.get(at).copied().is_some_and(space){at+=1}
        let start=at;
        if let Some(value)=hex(data,&mut at){if at>start+2{return value}}
    }
    Vec::new()
}
pub(super) fn certificate_encrypted(dict:&[u8])->Result<bool,String>{
    let text=latin1(dict);
    for pattern in [r"/Recipients\b",r"/Filter\s*/(?!Standard\b)"] {
        if TryJsRegex::new(pattern,"").map_err(|e|e.to_string())?.try_test(&text).map_err(|e|e.to_string())?{return Ok(true)}
    }
    Ok(false)
}
pub(super) fn permissions(dict:&[u8])->Result<i32,String>{
    let text=latin1(dict);let re=TryJsRegex::new(r"/P\s+(-?\d+)","").map_err(|e|e.to_string())?;
    let Some(found)=re.try_exec(&text).map_err(|e|e.to_string())? else{return Ok(-1)};
    let n=found.get(1).unwrap_or("-1").parse::<f64>().unwrap_or(-1.);
    Ok(jscompat::to_int32(n))
}
fn capture<'a>(text:&'a str,pattern:&str)->Result<Option<&'a str>,String>{
    let regex=TryJsRegex::new(pattern,"").map_err(|e|e.to_string())?;
    Ok(regex.try_exec(text).map_err(|e|e.to_string())?.and_then(|m|m.get(1)))
}
fn number(text:&str,name:&str,max:bool)->Result<i64,String>{
    let regex=TryJsRegex::new(&format!(r"/{name}\s+(-?\d+)"),"").map_err(|e|e.to_string())?;
    let mut value:Option<i64>=None;
    for found in regex.try_find_iter(text) {
        let found=found.map_err(|e|e.to_string())?;
        let n=found.get(1).unwrap_or("0").parse::<f64>().unwrap_or(0.0) as i64;
        value=Some(if max {value.map_or(n,|old|old.max(n))} else {n});
        if !max {break}
    }
    Ok(value.unwrap_or(0))
}
fn entry(text:&str,name:&str)->Result<Vec<u8>,String>{
    if let Some(h)=capture(text,&format!(r"/{name}\s*<([0-9a-fA-F\s]+)>"))? {
        let chars:Vec<u8>=h.bytes().filter(|b|b.is_ascii_hexdigit()).collect();
        return Ok(chars.chunks(2).map(|p|{
            let hi=(p[0] as char).to_digit(16).unwrap_or(0) as u8;
            let lo=p.get(1).and_then(|b|(*b as char).to_digit(16)).unwrap_or(0) as u8;
            hi*16+lo
        }).collect())
    }
    let Some(value)=capture(text,&format!(r"/{name}\s*\(((?:[^()\\]|\\.)*)\)"))? else{return Ok(Vec::new())};
    let chars:Vec<char>=value.chars().collect();let mut result=Vec::new();let mut i=0;
    while i<chars.len(){
        let c=chars[i];i+=1;
        if c!='\\'{result.push(c as u8);continue}
        if i==chars.len(){break}let c=chars[i];i+=1;
        result.push(match c {
            'n'=>10,'r'=>13,'t'=>9,'b'=>8,'f'=>12,
            '0'..='7'=>{let mut n=c as u32-'0' as u32;let mut count=1;while count<3&&i<chars.len()&&('0'..='7').contains(&chars[i]){n=n*8+chars[i] as u32-'0' as u32;i+=1;count+=1}n as u8},
            _=>c as u8,
        });
    }
    Ok(result)
}
/// Apply readEncryptEntries and encryptDictFor, retaining only their emitted entries.
pub(super) fn normalize(raw:&[u8])->Result<(lopdf::Dictionary,Vec<u8>),String>{
    use lopdf::{Dictionary,Object,StringFormat};
    let text=latin1(raw);let mut dict=Dictionary::new();let mut out=String::from("<<");
    let mut name=|key:&str,value:&str|{dict.set(key,Object::Name(value.as_bytes().to_vec()));out.push_str(&format!(" /{key} /{value}"));};
    let filter=capture(&text,r"/Filter\s*/(\w+)")?.unwrap_or("Standard");
    name("Filter",filter);drop(name);
    for key in ["V","R","Length","P"] {
        let n=number(&text,key,key=="Length")?;
        if key!="Length"||n>=40 {dict.set(key,Object::Integer(n));out.push_str(&format!(" /{key} {n}"));}
    }
    for key in ["O","U","OE","UE","Perms"] {
        let bytes=entry(&text,key)?;
        if (key=="O"||key=="U")||!bytes.is_empty(){
            out.push_str(&format!(" /{key} <"));
            for byte in &bytes {out.push_str(&format!("{byte:02X}"))}
            out.push('>');dict.set(key,Object::String(bytes,StringFormat::Hexadecimal));
        }
    }
    let re=TryJsRegex::new(r"/(\w+)\s*<<([^<>]*)>>","").map_err(|e|e.to_string())?;
    let mut filters:Vec<(String,Dictionary,String)>=Vec::new();
    for found in re.try_find_iter(&text) {
        let found=found.map_err(|e|e.to_string())?;let key=found.get(1).unwrap_or("");let inner=found.get(2).unwrap_or("");
        let cfm=capture(inner,r"/CFM\s*/(\w+)")?.unwrap_or("V2");
        let auth=capture(inner,r"/AuthEvent\s*/(\w+)")?;
        let length=capture(inner,r"/Length\s+(\d+)")?.and_then(|s|s.parse::<i64>().ok());
        let mut d=Dictionary::new();d.set("CFM",Object::Name(cfm.as_bytes().to_vec()));
        let mut serialized=format!("<< /CFM /{cfm}");
        if let Some(auth)=auth {d.set("AuthEvent",Object::Name(auth.as_bytes().to_vec()));serialized.push_str(&format!(" /AuthEvent /{auth}"));}
        if let Some(length)=length {d.set("Length",Object::Integer(length));serialized.push_str(&format!(" /Length {length}"));}
        serialized.push_str(" >>");
        if let Some(existing)=filters.iter_mut().find(|(n,_,_)|n==key){*existing=(key.into(),d,serialized)}else{filters.push((key.into(),d,serialized))}
    }
    if !filters.is_empty(){
        let mut cf=Dictionary::new();out.push_str(" /CF <<");
        for (key,value,serialized) in filters {out.push_str(&format!(" /{key} {serialized}"));cf.set(key,Object::Dictionary(value));}
        out.push_str(" >>");dict.set("CF",Object::Dictionary(cf));
    }
    for (key,pattern) in [("StmF",r"/StmF\s*/(\w+)"),("StrF",r"/StrF\s*/(\w+)")] {
        let value=capture(&text,pattern)?.unwrap_or("StdCF");
        dict.set(key,Object::Name(value.as_bytes().to_vec()));out.push_str(&format!(" /{key} /{value}"));
    }
    let metadata=!TryJsRegex::new(r"/EncryptMetadata\s+false","").map_err(|e|e.to_string())?.try_test(&text).map_err(|e|e.to_string())?;
    if !metadata {dict.set("EncryptMetadata",Object::Boolean(false));out.push_str(" /EncryptMetadata false");}
    out.push_str(" >>");Ok((dict,out.into_bytes()))
}
/// Serialize the direct values emitted by encryptDictFor.
pub(super) fn serialize_dictionary(dictionary:&lopdf::Dictionary)->Result<Vec<u8>,String>{
    fn object(out:&mut Vec<u8>,value:&lopdf::Object)->Result<(),String>{
        match value {
            lopdf::Object::Integer(n)=>out.extend_from_slice(n.to_string().as_bytes()),
            lopdf::Object::Boolean(v)=>out.extend_from_slice(if *v {b"true"} else {b"false"}),
            lopdf::Object::Name(name)=>{out.push(b'/');out.extend_from_slice(name);},
            lopdf::Object::String(bytes,_)=>{
                const HEX:&[u8;16]=b"0123456789ABCDEF";out.push(b'<');
                for &b in bytes {out.push(HEX[(b>>4) as usize]);out.push(HEX[(b&15) as usize]);}
                out.push(b'>');
            },
            lopdf::Object::Dictionary(dict)=>{
                out.extend_from_slice(b"<<");
                for (key,value) in dict.iter(){out.push(b' ');out.push(b'/');out.extend_from_slice(key);out.push(b' ');object(out,value)?;}
                out.extend_from_slice(b" >>");
            },
            _=>return Err("Unsupported encryption dictionary value".into()),
        }Ok(())
    }
    let mut out=Vec::new();object(&mut out,&lopdf::Object::Dictionary(dictionary.clone()))?;Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn finds_referenced_dictionary_and_certificates(){
        let raw=b"%PDF\n4 0 obj\n<< /Filter /Adobe.PubSec /Recipients [<01>] /P -44 >>\nendobj\ntrailer << /Encrypt 4 0 R >>";
        let dict=encrypt_dictionary(raw).unwrap();
        assert!(certificate_encrypted(dict).unwrap());
        assert_eq!(permissions(dict).unwrap(),-44);
        assert!(encrypt_dictionary(b"%PDF plain").is_none());
    }
    #[test] fn normalizes_regex_entries_and_discards_unrelated_keys(){
        let input=br"<< /R 4 /V 4 /Length 128 /Length 16 /O (a\053b) /U <abc> /Unused 3 /CF << /StdCF << /CFM /AESV2 /Length 16 >> >> >>";
        let (dict,bytes)=normalize(input).unwrap();
        assert_eq!(dict.get(b"P").unwrap().as_i64().unwrap(),0);
        assert_eq!(dict.get(b"Length").unwrap().as_i64().unwrap(),128);
        assert_eq!(dict.get(b"O").unwrap().as_str().unwrap(),b"a+b");
        assert_eq!(dict.get(b"U").unwrap().as_str().unwrap(),&[0xab,0xc0]);
        assert!(!String::from_utf8(bytes).unwrap().contains("/Unused"));
    }
    #[test] fn preserves_first_raw_file_id_and_odd_hex_nibble(){
        assert_eq!(file_id(b"/ID [<a b c> <1234>] /ID [<ff><ff>]"),Some([vec![0xab,0xc0],vec![0x12,0x34]]));
        assert!(file_id(b"/ID [<aa>] ").is_none());
        assert_eq!(first_id(b"/ID [<aa>]"),vec![0xaa]);
        assert_eq!(first_id(b"/ID [<>] /ID [<12>]"),vec![0x12]);
    }
}
