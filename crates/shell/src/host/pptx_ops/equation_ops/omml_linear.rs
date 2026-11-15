use jscompat::js_regex;

#[derive(Default)]
struct Node {
    name: String,
    attrs: String,
    text: String,
    children: Vec<usize>,
}

fn child<'a>(nodes: &'a [Node], node: &Node, name: &str) -> Option<&'a Node> {
    node.children.iter().map(|i| &nodes[*i]).find(|n| n.name == name)
}
fn attr(node: Option<&Node>) -> Option<&str> {
    node.and_then(|n| js_regex!(r#"\bm:val="([^"]*)""#).exec(&n.attrs).and_then(|m| m.get(1)))
}
fn group(s: String) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '.') { s } else { format!("({s})") }
}
fn linear(nodes: &[Node], n: &Node) -> String {
    let part = |name| child(nodes, n, name).map(|c| linear(nodes, c)).unwrap_or_default();
    let all = || n.children.iter().map(|i| linear(nodes, &nodes[*i])).collect::<String>();
    match n.name.as_str() {
        "m:t" => n.text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"),
        "m:rPr" | "m:ctrlPr" | "m:fPr" | "m:sSupPr" | "m:sSubPr" | "m:sSubSupPr" | "m:radPr" | "m:dPr" | "m:naryPr" | "m:accPr" | "m:barPr" | "m:mPr" | "m:funcPr" | "m:limLowPr" | "m:limUppPr" | "m:boxPr" | "m:borderBoxPr" | "m:groupChrPr" | "m:eqArrPr" | "m:oMathParaPr" => String::new(),
        "m:f" => format!("{}/{}", group(part("m:num")), group(part("m:den"))),
        "m:sSup" => format!("{}^{}", part("m:e"), group(part("m:sup"))),
        "m:sSub" => format!("{}_{}", part("m:e"), group(part("m:sub"))),
        "m:sSubSup" => format!("{}_{}^{}", part("m:e"), group(part("m:sub")), group(part("m:sup"))),
        "m:sPre" => format!("{}{}{}", group(part("m:sub")), group(part("m:sup")), part("m:e")),
        "m:rad" => format!("{}√({})", part("m:deg"), part("m:e")),
        "m:d" => {
            let pr = child(nodes, n, "m:dPr");
            let start = pr.and_then(|p| child(nodes, p, "m:begChr")).map(|p| attr(Some(p)).unwrap_or("")).unwrap_or("(");
            let end = pr.and_then(|p| child(nodes, p, "m:endChr")).map(|p| attr(Some(p)).unwrap_or("")).unwrap_or(")");
            let parts = n.children.iter().map(|i| &nodes[*i]).filter(|n| n.name == "m:e").map(|n| linear(nodes, n)).collect::<Vec<_>>().join(", ");
            format!("{start}{parts}{end}")
        }
        "m:nary" => {
            let chr = attr(child(nodes, n, "m:naryPr").and_then(|p| child(nodes, p, "m:chr"))).unwrap_or("∫");
            let sub = part("m:sub"); let sup = part("m:sup");
            format!("{chr}{}{} {}", if sub.is_empty() { String::new() } else { format!("_{}", group(sub)) }, if sup.is_empty() { String::new() } else { format!("^{}", group(sup)) }, part("m:e"))
        }
        "m:func" => format!("{} {}", part("m:fName"), part("m:e")),
        "m:limLow" => format!("{}_{}", part("m:e"), group(part("m:lim"))),
        "m:limUpp" => format!("{}^{}", part("m:e"), group(part("m:lim"))),
        "m:bar" => { let pos = attr(child(nodes, n, "m:barPr").and_then(|p| child(nodes, p, "m:pos"))); format!("{}{}", part("m:e"), if pos == Some("bot") { '\u{0332}' } else { '\u{0304}' }) }
        "m:acc" => format!("{}{}", part("m:e"), attr(child(nodes, n, "m:accPr").and_then(|p| child(nodes, p, "m:chr"))).unwrap_or("\u{0304}")),
        "m:m" => format!("[{}]", n.children.iter().map(|i| &nodes[*i]).filter(|n| n.name == "m:mr").map(|r| r.children.iter().map(|i| linear(nodes, &nodes[*i])).collect::<Vec<_>>().join(", ")).collect::<Vec<_>>().join("; ")),
        "m:eqArr" => n.children.iter().map(|i| &nodes[*i]).filter(|n| n.name == "m:e").map(|n| linear(nodes, n)).collect::<Vec<_>>().join("; "),
        _ => all(),
    }
}

pub(super) fn omml_to_text(xml: &str) -> String {
    let mut nodes = vec![Node::default()];
    let mut stack = vec![0usize];
    for m in js_regex!(r"</([\w:]+)\s*>|<([\w:]+)([^>]*?)(/?)>|([^<]+)", "g").find_iter(xml) {
        if m.get(1).is_some() { if stack.len() > 1 { stack.pop(); } }
        else if let Some(name) = m.get(2) {
            let index = nodes.len();
            nodes.push(Node { name: name.to_owned(), attrs: m.get(3).unwrap_or("").to_owned(), ..Node::default() });
            nodes[*stack.last().unwrap()].children.push(index);
            if m.get(4).unwrap_or("").is_empty() { stack.push(index); }
        } else if let Some(text) = m.get(5) { nodes[*stack.last().unwrap()].text.push_str(text); }
    }
    linear(&nodes, &nodes[0]).split_whitespace().collect::<Vec<_>>().join(" ")
}
