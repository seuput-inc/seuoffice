//! Read-only PDF signature classification and cryptographic verification.
mod cms;
mod pdf;

use chrono::TimeZone;
use openssl::hash::{Hasher,MessageDigest};
use openssl::pkey::Id;
use openssl::rsa::Padding;
use openssl::sign::Verifier;
use openssl::x509::X509;
use shell_contract::IpcValue;
use super::super::crypto;

fn text(value:impl Into<String>)->IpcValue{IpcValue::String(value.into())}
fn optional(value:Option<String>)->IpcValue{value.map(text).unwrap_or(IpcValue::Undefined)}
fn status(problems:&[&str])->&'static str {
    if problems.iter().any(|p|matches!(*p,"digest-mismatch"|"signature-invalid"|"malformed"|"signer-certificate-missing")){"invalid"}
    else if problems.iter().any(|p|matches!(*p,"document-changed-after-signing"|"certificate-expired"|"certificate-not-yet-valid"|"unsupported-algorithm")){"warning"}
    else if problems.iter().any(|p|matches!(*p,"self-signed"|"untrusted-issuer")){"untrusted"}else{"valid"}
}
fn digest(oid:&str)->Option<(MessageDigest,crypto::DigestAlgorithm,&'static str)>{match oid {
    "1.3.14.3.2.26"=>Some((MessageDigest::sha1(),crypto::DigestAlgorithm::Sha1,"SHA1")),
    "2.16.840.1.101.3.4.2.4"=>Some((MessageDigest::sha224(),crypto::DigestAlgorithm::Sha224,"SHA224")),
    "2.16.840.1.101.3.4.2.1"=>Some((MessageDigest::sha256(),crypto::DigestAlgorithm::Sha256,"SHA256")),
    "2.16.840.1.101.3.4.2.2"=>Some((MessageDigest::sha384(),crypto::DigestAlgorithm::Sha384,"SHA384")),
    "2.16.840.1.101.3.4.2.3"=>Some((MessageDigest::sha512(),crypto::DigestAlgorithm::Sha512,"SHA512")),_=>None}}
fn supported(oid:&str)->bool{matches!(oid,"1.2.840.113549.1.1.1"|"1.2.840.113549.1.1.5"|"1.2.840.113549.1.1.11"|"1.2.840.113549.1.1.12"|"1.2.840.113549.1.1.13"|"1.2.840.113549.1.1.14"|"1.2.840.10045.2.1"|"1.2.840.10045.4.1"|"1.2.840.10045.4.3.1"|"1.2.840.10045.4.3.2"|"1.2.840.10045.4.3.3"|"1.2.840.10045.4.3.4")}
fn signed_ranges(bytes:&[u8],range:[usize;4])->Option<(&[u8],&[u8],bool)>{
    let [start,len,next,tail]=range;let end=start.checked_add(len)?;let finish=next.checked_add(tail)?;
    if start!=0||end>next||finish>bytes.len()||bytes.get(end)!=Some(&b'<')||next.checked_sub(1).and_then(|i|bytes.get(i))!=Some(&b'>'){return None}
    Some((&bytes[start..end],&bytes[next..finish],bytes[finish..].iter().all(|b|matches!(*b,b'\n'|b'\r'|b' '))))
}
fn pdf_date(raw:&str)->Option<String>{
    let bytes=raw.as_bytes();if !bytes.starts_with(b"D:"){return None}let mut i=2usize;
    fn digits(bytes:&[u8],i:&mut usize,n:usize)->Option<u32>{let slice=bytes.get(*i..(*i).checked_add(n)?)?;if !slice.iter().all(u8::is_ascii_digit){return None}*i+=n;std::str::from_utf8(slice).ok()?.parse().ok()}
    let year=digits(bytes,&mut i,4)? as i32;let mut values=[1u32,1,0,0,0];for v in &mut values{let saved=i;if let Some(n)=digits(bytes,&mut i,2){*v=n}else{i=saved;break}}
    let [month,day,hour,minute,second]=values;if !(1..=12).contains(&month)||!(1..=31).contains(&day)||hour>24||minute>59||second>59||(hour==24&&(minute!=0||second!=0)){return None}
    let mut offset=0i32;if let Some(sign @ (b'+'|b'-'))=bytes.get(i).copied(){let mut pos=i+1;if let Some(hours)=digits(bytes,&mut pos,2){if bytes.get(pos)==Some(&b'\''){pos+=1}let minutes=digits(bytes,&mut pos,2).unwrap_or(0);if hours>23||minutes>59{return None}offset=(hours*3600+minutes*60) as i32;if sign==b'-'{offset = -offset;}}}
    let day0=chrono::NaiveDate::from_ymd_opt(year,month,1)?;let date=day0.checked_add_signed(chrono::Duration::days(i64::from(day-1)))?;
    let time=date.and_hms_opt(hour%24,minute,second)?.checked_add_signed(chrono::Duration::days(if hour==24 {1}else{0}))?;
    let datetime=chrono::FixedOffset::east_opt(offset)?.from_local_datetime(&time).single()?;
    Some(datetime.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis,true))
}
fn get_bool(value:&IpcValue,key:&str)->bool{match value {IpcValue::Object(v)=>v.iter().find(|(k,_)|k==key).is_some_and(|(_,v)|matches!(v,IpcValue::Bool(true))),_=>false}}
fn verify_detached(cert:&X509,md:MessageDigest,range1:&[u8],range2:&[u8],signature:&[u8])->bool {
    let Ok(key)=cert.public_key() else{return false};if key.id()!=Id::RSA && key.id()!=Id::EC{return false}
    let Ok(mut verifier)=Verifier::new(md,&key) else{return false};if key.id()==Id::RSA && verifier.set_rsa_padding(Padding::PKCS1).is_err(){return false}
    verifier.update(range1).is_ok() && verifier.update(range2).is_ok() && verifier.verify(signature).unwrap_or(false)
}
fn at_time(cert:&X509,signed:Option<&str>)->Result<Option<&'static str>,String>{
    let seconds=match signed {Some(value)=>chrono::DateTime::parse_from_rfc3339(value).map_err(|e|e.to_string())?.timestamp(),None=>chrono::Utc::now().timestamp()};
    let at=openssl::asn1::Asn1Time::from_unix(seconds).map_err(|e|e.to_string())?;
    if at.compare(cert.not_before()).map_err(|e|e.to_string())?==std::cmp::Ordering::Less {Ok(Some("certificate-not-yet-valid"))}
    else if at.compare(cert.not_after()).map_err(|e|e.to_string())?==std::cmp::Ordering::Greater {Ok(Some("certificate-expired"))}else{Ok(None)}
}
fn entry(ctx:&super::super::crypto::pdf_lib::Context,field:&pdf::Field<'_>,bytes:&[u8])->Result<IpcValue,String>{
    let value=field.value;let sub=pdf::name(ctx,value,b"SubFilter").unwrap_or("").to_owned();let kind=if sub=="ETSI.RFC3161"||pdf::name(ctx,value,b"Type")==Some("DocTimeStamp"){"timestamp"}else{"signature"};
    let claimed=pdf::text(ctx,value,b"M").and_then(|s|pdf_date(&s));let mut signing_time=claimed.clone();let mut signing_time_signed=false;let mut digest_name=None;let mut covers=false;let mut signer_info=None;let mut chain_info=Vec::new();let mut trusted=false;let mut problems=Vec::new();
    let outcome=(|| -> Result<(),String> {
        let Some(range)=pdf::byte_range(ctx,value) else {problems.push("malformed");return Ok(())};
        let Some(contents)=pdf::contents(ctx,value) else {problems.push("malformed");return Ok(())};
        let Some((first,second,whole))=signed_ranges(bytes,range) else {problems.push("malformed");return Ok(())};covers=whole;
        if kind=="timestamp" {problems.push("unsupported-algorithm");return Ok(())}
        let parsed=match cms::parse(&contents){Ok(parsed)=>parsed,Err(())=>{problems.push("malformed");return Ok(())}};
        if let Some(time)=parsed.signing_time.clone(){signing_time=Some(time);signing_time_signed=true;}
        let algorithm=digest(&parsed.digest_oid);digest_name=algorithm.as_ref().map(|(_,_,name)|*name);
        let Some(index)=parsed.signer else {problems.push("signer-certificate-missing");return Ok(())};
        let signer=&parsed.certificates[index];let cert=X509::from_der(signer).map_err(|e|e.to_string())?;
        let (chain,trust)=crypto::verified_chain_and_trust(signer,&parsed.certificates)?;trusted=trust;
        signer_info=Some(crypto::certificate_info(signer)?);chain_info=chain.iter().map(|der|crypto::certificate_info(der)).collect::<Result<Vec<_>,_>>()?;
        let Some((md,algorithm,_))=algorithm.filter(|_|supported(&parsed.signature_oid)) else {problems.push("unsupported-algorithm");return Ok(())};
        let mut hasher=Hasher::new(md).map_err(|e|e.to_string())?;hasher.update(first).map_err(|e|e.to_string())?;hasher.update(second).map_err(|e|e.to_string())?;let hash=hasher.finish().map_err(|e|e.to_string())?;
        let valid=if let Some(attrs)=parsed.attributes.as_deref(){if parsed.message_digest.as_deref()!=Some(hash.as_ref()){problems.push("digest-mismatch");false}else{crypto::verify_signature(signer,algorithm,attrs,&parsed.signature).unwrap_or(false)}}else{verify_detached(&cert,md,first,second,&parsed.signature)};
        if problems.is_empty()&&!valid{problems.push("signature-invalid")}
        if problems.is_empty(){if !covers{problems.push("document-changed-after-signing")}if let Some(problem)=at_time(&cert,parsed.signing_time.as_deref())?{problems.push(problem)}if !trusted{problems.push(if chain.len()==1 && chain_info.last().is_some_and(|v|get_bool(v,"selfSigned")){"self-signed"}else{"untrusted-issuer"});}}
        Ok(())
    })();outcome?;
    let mut values=vec![
        ("fieldName".into(),text(&field.name)),("pageIndex".into(),IpcValue::Number(pdf::page_index(ctx,field) as f64)),("kind".into(),text(kind)),("status".into(),text(status(&problems))),
        ("problems".into(),IpcValue::Array(problems.iter().map(|p|text(*p)).collect())),("subFilter".into(),text(sub)),
        ("signerName".into(),optional(pdf::text(ctx,value,b"Name"))),("reason".into(),optional(pdf::text(ctx,value,b"Reason"))),("location".into(),optional(pdf::text(ctx,value,b"Location"))),("contactInfo".into(),optional(pdf::text(ctx,value,b"ContactInfo"))),
        ("signingTime".into(),optional(signing_time)),("signingTimeSigned".into(),IpcValue::Bool(signing_time_signed)),("coversWholeDocument".into(),IpcValue::Bool(covers)),
        ("certifiedLevel".into(),pdf::certified(ctx,value).map_or(IpcValue::Undefined,|v|IpcValue::Number(f64::from(v)))),("chain".into(),IpcValue::Array(chain_info)),("trusted".into(),IpcValue::Bool(trusted))];
    if let Some(name)=digest_name{values.push(("digestAlgorithm".into(),text(name)));}if let Some(signer)=signer_info{values.push(("signer".into(),signer));}Ok(IpcValue::Object(values))
}
pub(super) fn read_pdf_signatures(bytes:&[u8])->Result<IpcValue,String>{
    if !bytes.windows(b"/ByteRange".len()).any(|part|part==b"/ByteRange"){return Ok(IpcValue::Array(Vec::new()))}
    let Some(ctx)=pdf::context(bytes) else{return Ok(IpcValue::Array(Vec::new()))};
    let fields=pdf::fields(&ctx);fields.iter().map(|field|entry(&ctx,field,bytes)).collect::<Result<Vec<_>,_>>().map(IpcValue::Array)
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn ranges_reject_wrapping_and_allow_only_original_trailing_whitespace(){let bytes=b"abc<1234>def \r\n";assert!(signed_ranges(bytes,[0,3,9,3]).unwrap().2);assert!(signed_ranges(bytes,[0,2,9,3]).is_none());assert!(!signed_ranges(b"abc<1234>def\t",[0,3,9,3]).unwrap().2);}
    #[test] fn severity_orders_invalid_over_warnings(){assert_eq!(status(&["self-signed"]),"untrusted");assert_eq!(status(&["certificate-expired","self-signed"]),"warning");assert_eq!(status(&["digest-mismatch","certificate-expired"]),"invalid");}
    #[test] fn parses_pdf_dates_with_optional_offsets(){assert_eq!(pdf_date("D:20240102030405+09'30'"),Some("2024-01-01T17:34:05.000Z".into()));assert_eq!(pdf_date("D:2024"),Some("2024-01-01T00:00:00.000Z".into()));}
    #[test] fn reads_a_real_pdf_timestamp_field_and_rejects_bad_ranges(){
        let mut doc=String::from("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [4 0 R] >> >> endobj\n2 0 obj << /Type /Pages /Count 1 /Kids [3 0 R] >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /Annots [4 0 R] >> endobj\n4 0 obj << /FT /Sig /T (Timestamp) /P 3 0 R /V 5 0 R >> endobj\n5 0 obj << /Type /DocTimeStamp /SubFilter /ETSI.RFC3161 /ByteRange [0 1111111111 2222222222 3333333333] /Contents <DEAD> >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF");
        let start=doc.find("<DEAD>").unwrap();let end=start+6;let tail=doc.len()-end;
        doc=doc.replace("1111111111",&format!("{start:010}")).replace("2222222222",&format!("{end:010}")).replace("3333333333",&format!("{tail:010}"));
        let IpcValue::Array(entries)=read_pdf_signatures(doc.as_bytes()).unwrap() else{panic!("expected signature list")};
        assert_eq!(entries.len(),1);
        let IpcValue::Object(entry)=&entries[0] else{panic!("expected signature entry")};
        assert!(entry.iter().any(|(k,v)|k=="kind" && matches!(v,IpcValue::String(s) if s=="timestamp")));
        assert!(entry.iter().any(|(k,v)|k=="status" && matches!(v,IpcValue::String(s) if s=="warning")));
        assert!(signed_ranges(doc.as_bytes(),[0,start-1,end,tail]).is_none());
    }
}
