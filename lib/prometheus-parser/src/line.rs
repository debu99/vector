//! Parse a single line of Prometheus text format.

use crate::{IResult, ParserError};
use nom::{
    branch::alt,
    bytes::complete::{is_not, tag, take_while, take_while1},
    character::complete::char,
    combinator::{map, opt, value},
    error::ParseError,
    multi::{fold_many0, separated_list},
    number::complete::double,
    sequence::{delimited, pair, preceded, tuple},
};
use std::collections::BTreeMap;

type NomError<'a> = nom::Err<(&'a str, nom::error::ErrorKind)>;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    Summary,
    Untyped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub metric_name: String,
    pub kind: MetricKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

impl Metric {
    /// Parse a single line with format
    ///
    /// ``` text
    /// metric_name [
    ///   "{" label_name "=" `"` label_value `"` { "," label_name "=" `"` label_value `"` } [ "," ] "}"
    /// ] value [ timestamp ]
    /// ```
    ///
    /// We don't parse timestamp.
    fn parse(input: &str) -> IResult<&str, Self> {
        let input = trim_space(input);
        let (input, name) = parse_name(input)?;
        let (input, labels) = Self::parse_labels(input)?;
        let (input, value) = Self::parse_value(input)?;
        Ok((
            input,
            Metric {
                name,
                labels,
                value,
            },
        ))
    }

    /// Float value, and +Inf, -Int, Nan.
    pub fn parse_value(input: &str) -> IResult<&str, f64> {
        let input = trim_space(input);
        alt((
            value(f64::INFINITY, tag("+Inf")),
            value(f64::NEG_INFINITY, tag("-Inf")),
            value(f64::NAN, tag("Nan")),
            double,
        ))(input)
        .map_err(|_: NomError| {
            ParserError::ParseFloatError {
                input: input.to_owned(),
            }
            .into()
        })
    }

    fn parse_name_value(input: &str) -> IResult<&str, (String, String)> {
        map(
            tuple((
                parse_name,
                preceded(sp, char('=')),
                Self::parse_escaped_string,
            )),
            |(name, _, value)| (name, value),
        )(input)
    }

    fn parse_labels_inner(input: &str) -> IResult<&str, BTreeMap<String, String>> {
        let (input, list) = separated_list(preceded(sp, char(',')), Self::parse_name_value)(input)?;
        let (input, _) = opt(preceded(sp, char(',')))(input)?;
        Ok((input, list.into_iter().collect()))
    }

    /// Parse `{label_name="value",...}`
    fn parse_labels(input: &str) -> IResult<&str, BTreeMap<String, String>> {
        let input = trim_space(input);

        match opt(char('{'))(input) {
            Ok((input, None)) => Ok((input, BTreeMap::new())),
            Ok((input, Some(_))) => {
                let (input, labels) = Self::parse_labels_inner(input)?;
                let (input, _) = preceded(sp, char('}'))(input).map_err(|_: NomError| {
                    ParserError::ExpectedToken {
                        expected: "}",
                        input: input.to_owned(),
                    }
                })?;
                Ok((input, labels))
            }
            Err(e) => Err(e),
        }
    }

    /// Parse `'"' string_content '"'`. `string_content` can contain any unicode characters,
    /// backslash (`\`), double-quote (`"`), and line feed (`\n`) characters have to be
    /// escaped as `\\`, `\"`, and `\n`, respectively.
    fn parse_escaped_string(input: &str) -> IResult<&str, String> {
        #[derive(Debug)]
        enum StringFragment<'a> {
            Literal(&'a str),
            EscapedChar(char),
        }

        let parse_string_fragment = alt((
            map(is_not("\"\\"), StringFragment::Literal),
            map(
                preceded(
                    char('\\'),
                    alt((
                        value('\n', char('n')),
                        value('"', char('"')),
                        value('\\', char('\\')),
                    )),
                ),
                StringFragment::EscapedChar,
            ),
        ));

        let input = trim_space(input);

        let build_string = fold_many0(
            parse_string_fragment,
            String::new(),
            |mut result, fragment| {
                match fragment {
                    StringFragment::Literal(s) => result.push_str(s),
                    StringFragment::EscapedChar(c) => result.push(c),
                }
                result
            },
        );

        fn match_quote(input: &str) -> IResult<&str, char> {
            char('"')(input).map_err(|_: NomError| {
                ParserError::ExpectedToken {
                    expected: "\"",
                    input: input.to_owned(),
                }
                .into()
            })
        }

        delimited(match_quote, build_string, match_quote)(input)
    }
}

impl Header {
    /// `# TYPE <metric_name> <metric_type>`
    fn parse(input: &str) -> IResult<&str, Self> {
        let input = trim_space(input);
        let (input, _) = tag("#")(input).map_err(|_: NomError| ParserError::ExpectedToken {
            expected: "#",
            input: input.to_owned(),
        })?;
        let input = trim_space(input);
        let (input, _) = tag("TYPE")(input).map_err(|_: NomError| ParserError::ExpectedToken {
            expected: "TYPE",
            input: input.to_owned(),
        })?;
        let (input, metric_name) = parse_name(input)?;
        let input = trim_space(input);
        let (input, kind) = alt((
            value(MetricKind::Counter, tag("counter")),
            value(MetricKind::Gauge, tag("gauge")),
            value(MetricKind::Summary, tag("summary")),
            value(MetricKind::Histogram, tag("histogram")),
            value(MetricKind::Untyped, tag("untyped")),
        ))(input)
        .map_err(|_: NomError| ParserError::InvalidMetricKind {
            input: input.to_owned(),
        })?;
        Ok((input, Header { metric_name, kind }))
    }
}

/// Each line of Prometheus text format.
/// We discard empty lines, comments, and timestamps.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Header(Header),
    Metric(Metric),
}

impl Line {
    /// Parse a single line. Return `None` if it is a comment or an empty line.
    fn parse_inner(input: &str) -> IResult<&str, Option<Self>> {
        let input = input.trim();
        if input.is_empty() {
            return Ok((input, None));
        }
        alt((
            map(Metric::parse, |r| Some(Line::Metric(r))),
            map(Header::parse, |r| Some(Line::Header(r))),
            value(None, char('#')),
        ))(input)
    }

    // TODO: use IResult
    pub fn parse(input: &str) -> Result<Option<Self>, ParserError> {
        Self::parse_inner(input)
            .map(|(_, line)| line)
            .map_err(From::from)
    }
}

/// Name matches the regex `[a-zA-Z_][a-zA-Z0-9_]*`.
fn parse_name(input: &str) -> IResult<&str, String> {
    let input = trim_space(input);
    let (input, (a, b)) = pair(
        take_while1(|c: char| c.is_alphabetic() || c == '_'),
        take_while(|c: char| c.is_alphanumeric() || c == '_'),
    )(input)
    .map_err(|_: NomError| ParserError::ParseNameError {
        input: input.to_owned(),
    })?;
    Ok((input, a.to_owned() + b))
}

fn trim_space(input: &str) -> &str {
    input.trim_start_matches(|c| c == ' ' || c == '\t')
}

fn sp<'a, E: ParseError<&'a str>>(i: &'a str) -> nom::IResult<&'a str, &'a str, E> {
    take_while(|c| c == ' ' || c == '\t')(i)
}

#[cfg(test)]
mod test {
    use super::*;

    macro_rules! map {
        ($($key:expr => $value:expr),*) => {
            {
                #[allow(unused_mut)]
                let mut m = ::std::collections::BTreeMap::new();
                $(
                    m.insert($key.into(), $value.into());
                )*
                m
            }
        };
    }

    #[test]
    fn test_parse_escaped_string() {
        fn wrap(s: &str) -> String {
            format!("  \t \"{}\"  .", s)
        }

        // parser should not consume more that it needed
        let tail = "  .";

        let input = wrap("");
        let (left, r) = Metric::parse_escaped_string(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "");

        let input = wrap(r#"a\\ asdf"#);
        let (left, r) = Metric::parse_escaped_string(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "a\\ asdf");

        let input = wrap(r#"\"\""#);
        let (left, r) = Metric::parse_escaped_string(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "\"\"");

        let input = wrap(r#"\"\\\n"#);
        let (left, r) = Metric::parse_escaped_string(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "\"\\\n");

        let input = wrap(r#"\\n"#);
        let (left, r) = Metric::parse_escaped_string(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "\\n");

        let input = wrap(r#"  😂  "#);
        let (left, r) = Metric::parse_escaped_string(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "  😂  ");
    }

    #[test]
    fn test_parse_name() {
        fn wrap(s: &str) -> String {
            format!("  \t {}  .", s)
        }
        let tail = "  .";

        let input = wrap("abc_def");
        let (left, r) = parse_name(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "abc_def");

        let input = wrap("__9A0bc_def__");
        let (left, r) = parse_name(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, "__9A0bc_def__");

        let input = wrap("99");
        assert!(parse_name(&input).is_err());
    }

    #[test]
    fn test_parse_header() {
        fn wrap(s: &str) -> String {
            format!("  \t {}  .", s)
        }
        let tail = "  .";

        let input = wrap("#  TYPE abc_def counter");
        let (left, r) = Header::parse(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(
            r,
            Header {
                metric_name: "abc_def".into(),
                kind: MetricKind::Counter,
            }
        );

        let input = wrap("#TYPE \t abc_def \t gauge");
        let (left, r) = Header::parse(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(
            r,
            Header {
                metric_name: "abc_def".into(),
                kind: MetricKind::Gauge,
            }
        );

        let input = wrap("# TYPE abc_def histogram");
        let (left, r) = Header::parse(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(
            r,
            Header {
                metric_name: "abc_def".into(),
                kind: MetricKind::Histogram,
            }
        );

        let input = wrap("# TYPE abc_def summary");
        let (left, r) = Header::parse(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(
            r,
            Header {
                metric_name: "abc_def".into(),
                kind: MetricKind::Summary,
            }
        );

        let input = wrap("# TYPE abc_def untyped");
        let (left, r) = Header::parse(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(
            r,
            Header {
                metric_name: "abc_def".into(),
                kind: MetricKind::Untyped,
            }
        );
    }

    #[test]
    fn test_parse_value() {
        fn wrap(s: &str) -> String {
            format!("  \t {}  .", s)
        }
        let tail = "  .";

        let input = wrap("+Inf");
        let (left, r) = Metric::parse_value(&input).unwrap();
        assert_eq!(left, tail);
        assert!(r.is_infinite() && r.is_sign_positive());

        let input = wrap("-Inf");
        let (left, r) = Metric::parse_value(&input).unwrap();
        assert_eq!(left, tail);
        assert!(r.is_infinite() && r.is_sign_negative());

        let input = wrap("Nan");
        let (left, r) = Metric::parse_value(&input).unwrap();
        assert_eq!(left, tail);
        assert!(r.is_nan());

        let tests = [
            ("0", 0.0),
            ("0.25", 0.25),
            ("-10.25", -10.25),
            ("-10e-25", -10e-25),
            ("-10e+25", -10e+25),
            ("2020", 2020.0),
            ("1.", 1.),
        ];
        for (text, value) in &tests {
            let input = wrap(text);
            let (left, r) = Metric::parse_value(&input).unwrap();
            assert_eq!(left, tail);
            assert_eq!(r, *value);
        }
    }

    #[test]
    fn test_parse_labels() {
        fn wrap(s: &str) -> String {
            format!("  \t {}  .", s)
        }
        let tail = "  .";

        let input = wrap("{}");
        let (left, r) = Metric::parse_labels(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, map! {});

        let input = wrap(r#"{name="value"}"#);
        let (left, r) = Metric::parse_labels(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, map! { "name" => "value" });

        let input = wrap(r#"{name="value",}"#);
        let (left, r) = Metric::parse_labels(&input).unwrap();
        assert_eq!(left, tail);
        assert_eq!(r, map! { "name" => "value" });

        let input = wrap(r#"{ name = "" ,b="a=b" , a="},", _c = "\""}"#);
        let (left, r) = Metric::parse_labels(&input).unwrap();
        assert_eq!(
            r,
            map! {"name" => "", "a" => "},", "b" => "a=b", "_c" => "\""}
        );
        assert_eq!(left, tail);

        let input = wrap("100");
        let (left, r) = Metric::parse_labels(&input).unwrap();
        assert_eq!(left, "100".to_owned() + &tail);
        assert_eq!(r, map! {});

        // We don't allow these values

        let input = wrap(r#"{name="value}"#);
        let result = Metric::parse_labels(&input);
        assert!(
            result.is_err() && !matches!(result, Err(nom::Err::Error(ParserError::Nom { .. })))
        );

        let input = wrap(r#"{ a="b" c="d" }"#);
        assert!(Metric::parse_labels(&input).is_err());

        let input = wrap(r#"{ a="b" ,, c="d" }"#);
        assert!(Metric::parse_labels(&input).is_err());
    }

    #[test]
    fn test_parse_line() {
        let input = r##"
            # HELP http_requests_total The total number of HTTP requests.
            # TYPE http_requests_total counter
            http_requests_total{method="post",code="200"} 1027 1395066363000
            http_requests_total{method="post",code="400"}    3 1395066363000

            # Escaping in label values:
            msdos_file_access_time_seconds{path="C:\\DIR\\FILE.TXT",error="Cannot find file:\n\"FILE.TXT\""} 1.458255915e9

            # Minimalistic line:
            metric_without_timestamp_and_labels 12.47

            # A weird metric from before the epoch:
            something_weird{problem="division by zero"} +Inf -3982045

            # A histogram, which has a pretty complex representation in the text format:
            # HELP http_request_duration_seconds A histogram of the request duration.
            # TYPE http_request_duration_seconds histogram
            http_request_duration_seconds_bucket{le="0.05"} 24054
            http_request_duration_seconds_bucket{le="0.1"} 33444
            http_request_duration_seconds_bucket{le="0.2"} 100392
            http_request_duration_seconds_bucket{le="0.5"} 129389
            http_request_duration_seconds_bucket{le="1"} 133988
            http_request_duration_seconds_bucket{le="+Inf"} 144320
            http_request_duration_seconds_sum 53423
            http_request_duration_seconds_count 144320

            # Finally a summary, which has a complex representation, too:
            # HELP rpc_duration_seconds A summary of the RPC duration in seconds.
            # TYPE rpc_duration_seconds summary
            rpc_duration_seconds{quantile="0.01"} 3102
            rpc_duration_seconds{quantile="0.05"} 3272
            rpc_duration_seconds{quantile="0.5"} 4773
            rpc_duration_seconds{quantile="0.9"} 9001
            rpc_duration_seconds{quantile="0.99"} 76656
            rpc_duration_seconds_sum 1.7560473e+07
            rpc_duration_seconds_count 2693
            "##;
        assert!(input.lines().map(Line::parse).all(|r| r.is_ok()));
    }
}