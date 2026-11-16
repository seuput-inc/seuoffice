use jscompat::TryJsRegex;

#[derive(Clone)]
enum Token {
    Number(String),
    Field(String),
    Op(char),
    Left,
    Right,
}
impl Token {
    fn text(&self) -> &str {
        match self {
            Token::Number(v) | Token::Field(v) => v,
            Token::Op('+') => "+",
            Token::Op('-') => "-",
            Token::Op('*') => "*",
            Token::Op('/') => "/",
            Token::Left => "(",
            Token::Right => ")",
            Token::Op(_) => "",
        }
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let numeric = TryJsRegex::new(r"^\d*\.?\d+(?:[eE][+-]?\d+)?", "").map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    let mut at = 0;
    while at < input.len() {
        let ch = input[at..]
            .chars()
            .next()
            .ok_or("The formula ended unexpectedly.")?;
        if ch.is_whitespace() {
            at += ch.len_utf8();
            continue;
        }
        if "+-*/".contains(ch) {
            result.push(Token::Op(ch));
            at += 1;
            continue;
        }
        if ch == '(' {
            result.push(Token::Left);
            at += 1;
            continue;
        }
        if ch == ')' {
            result.push(Token::Right);
            at += 1;
            continue;
        }
        if ch == '\'' {
            at += 1;
            let mut name = String::new();
            loop {
                let Some(next) = input[at..].chars().next() else {
                    return Err("Unclosed quote in the formula.".into());
                };
                at += next.len_utf8();
                if next == '\'' {
                    if input[at..].starts_with('\'') {
                        name.push('\'');
                        at += 1;
                        continue;
                    }
                    break;
                }
                name.push(next);
            }
            if name.trim().is_empty() {
                return Err("The formula references an empty field name.".into());
            }
            result.push(Token::Field(name.trim().to_owned()));
            continue;
        }
        if ch.is_ascii_digit() || ch == '.' {
            let found = numeric
                .try_find_iter(&input[at..])
                .next()
                .transpose()
                .map_err(|e| e.to_string())?;
            if let Some(found) = found
                && let Ok(value) = found.as_str().parse::<f64>()
                && value.is_finite()
            {
                        result.push(Token::Number(jscompat::number_to_string(value)));
                        at += found.end();
                        continue;
                    }
            let fragment: String = input[at..].chars().take(8).collect();
            return Err(format!(
                "Cannot parse a number in the formula: \"{fragment}\""
            ));
        }
        let start = at;
        while at < input.len() {
            let next = input[at..]
                .chars()
                .next()
                .ok_or("The formula ended unexpectedly.")?;
            if next.is_whitespace() || "+-*/()'".contains(next) {
                break;
            }
            at += next.len_utf8();
        }
        if at == start {
            return Err(format!("Unrecognized character in the formula: \"{ch}\""));
        }
        result.push(Token::Field(input[start..at].to_owned()));
    }
    Ok(result)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    at: usize,
    fields: &'a [String],
}
impl Parser<'_> {
    fn next(&mut self) -> Result<Token, String> {
        let token = self
            .tokens
            .get(self.at)
            .cloned()
            .ok_or("The formula ended unexpectedly.")?;
        self.at += 1;
        Ok(token)
    }
    fn expression(&mut self) -> Result<String, String> {
        let mut left = self.term()?;
        while matches!(
            self.tokens.get(self.at),
            Some(Token::Op('+') | Token::Op('-'))
        ) {
            let Token::Op(op) = self.next()? else {
                unreachable!()
            };
            left = format!("({left}{op}{})", self.term()?);
        }
        Ok(left)
    }
    fn term(&mut self) -> Result<String, String> {
        let mut left = self.unary()?;
        while matches!(
            self.tokens.get(self.at),
            Some(Token::Op('*') | Token::Op('/'))
        ) {
            let Token::Op(op) = self.next()? else {
                unreachable!()
            };
            left = format!("({left}{op}{})", self.unary()?);
        }
        Ok(left)
    }
    fn unary(&mut self) -> Result<String, String> {
        match self.tokens.get(self.at) {
            Some(Token::Op('-')) => {
                self.at += 1;
                Ok(format!("(-{})", self.unary()?))
            }
            Some(Token::Op('+')) => {
                self.at += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }
    fn primary(&mut self) -> Result<String, String> {
        match self.next()? {
            Token::Number(number) => Ok(number),
            Token::Field(field) => {
                let key = field.trim().to_lowercase();
                let canonical = self
                    .fields
                    .iter()
                    .rev()
                    .find(|name| name.trim().to_lowercase() == key)
                    .ok_or_else(|| {
                        format!("The formula references a nonexistent field \"{field}\".")
                    })?;
                Ok(format!("'{}'", canonical.replace('\'', "''")))
            }
            Token::Left => {
                let inner = self.expression()?;
                if !matches!(self.next()?, Token::Right) {
                    return Err("Unclosed parenthesis in the formula.".into());
                }
                Ok(inner)
            }
            token => Err(format!("Unexpected \"{}\" in the formula.", token.text())),
        }
    }
}

pub(in crate::host::d1::workbook_save_planner) fn normalize(
    formula: &str,
    fields: &[String],
) -> Result<String, String> {
    let input = formula.trim_start_matches(jscompat::is_js_space);
    let input = input.strip_prefix('=').unwrap_or(formula);
    let mut parser = Parser {
        tokens: tokenize(input)?,
        at: 0,
        fields,
    };
    let expression = parser.expression()?;
    if let Some(token) = parser.tokens.get(parser.at) {
        return Err(format!(
            "The formula cannot be parsed further at \"{}\".",
            token.text()
        ));
    }
    Ok(expression)
}

#[cfg(test)]
mod tests {
    use super::normalize;
    #[test]
    fn canonicalizes_references_and_respects_precedence() {
        let fields = vec!["Revenue".to_owned(), "Unit Cost".to_owned()];
        assert_eq!(
            normalize("=revenue-2*'unit cost'", &fields).unwrap(),
            "('Revenue'-(2*'Unit Cost'))"
        );
        assert_eq!(
            normalize("Revenue+Missing", &fields).unwrap_err(),
            "The formula references a nonexistent field \"Missing\"."
        );
    }
}
