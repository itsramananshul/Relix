use std::collections::{HashMap, HashSet};

#[derive(Debug)]
#[allow(dead_code)]
pub struct CliParser {
    pub args: Vec<String>,
    pub flags: HashSet<String>,
    pub options: HashMap<String, String>,
    pub arguments: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CliParseError {
    #[error("missing value after option `-{0}`")]
    MissingOptionValue(String),
    #[error("empty argument is not a valid flag")]
    EmptyArgument,
}

impl CliParser {
    /// Parse `args` (typically `std::env::args().collect()`)
    /// into flags, options, and positional arguments. Returns
    /// an error instead of panicking on malformed input so
    /// callers (including network-reachable surfaces that may
    /// invoke the SOL parser via embedded input) can surface
    /// a real diagnostic rather than abort the process.
    pub fn try_from(args: Vec<String>) -> Result<CliParser, CliParseError> {
        let mut flags = HashSet::new();
        let mut options = HashMap::new();
        let mut arguments = Vec::new();

        let mut iter = args.iter().skip(1);
        while let Some(arg) = iter.next() {
            // SAFETY: bytes-based inspection so an empty string
            // or one too short to read 2 chars never panics.
            let bytes = arg.as_bytes();
            if bytes.is_empty() {
                return Err(CliParseError::EmptyArgument);
            }
            if bytes[0] == b'-' {
                if bytes.len() >= 2 && bytes[1] == b'-' {
                    // double dash → flag
                    let flag = arg[2..].to_string();
                    flags.insert(flag);
                } else {
                    // single dash → option that takes a value
                    let option = arg[1..].to_string();
                    match iter.next() {
                        Some(value) => {
                            options.insert(option, value.to_string());
                        }
                        None => return Err(CliParseError::MissingOptionValue(option)),
                    }
                }
            } else {
                arguments.push(arg.to_string());
            }
        }

        Ok(CliParser {
            args,
            flags,
            options,
            arguments,
        })
    }

    /// Back-compat wrapper around [`Self::try_from`] for the
    /// historical infallible call sites. Prints the parse
    /// error to stderr and aborts the process — only safe in
    /// `main` of the standalone `sol` binary, where this was
    /// always the behaviour. Network-reachable callers must
    /// use `try_from` and propagate the error.
    pub fn from(args: Vec<String>) -> CliParser {
        match Self::try_from(args) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("sol: {e}");
                std::process::exit(2);
            }
        }
    }

    pub fn flag_value(&self, flag: &str) -> bool {
        return self.flags.contains(&flag.to_string());
    }
    #[allow(dead_code)]
    pub fn option_value(&self, flag: &str, default_value: &str) -> String {
        return self
            .options
            .get(&flag.to_string())
            .unwrap_or(&default_value.to_string())
            .to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_rejects_missing_option_value() {
        let args = vec!["sol".to_string(), "-x".to_string()];
        let err = CliParser::try_from(args).unwrap_err();
        assert!(matches!(err, CliParseError::MissingOptionValue(ref o) if o == "x"));
    }

    #[test]
    fn try_from_rejects_empty_argument() {
        let args = vec!["sol".to_string(), "".to_string()];
        let err = CliParser::try_from(args).unwrap_err();
        assert!(matches!(err, CliParseError::EmptyArgument));
    }

    #[test]
    fn try_from_parses_flags_options_and_positionals() {
        let args = vec![
            "sol".to_string(),
            "--debug".to_string(),
            "-out".to_string(),
            "result.txt".to_string(),
            "input.sol".to_string(),
        ];
        let p = CliParser::try_from(args).expect("parse");
        assert!(p.flag_value("debug"));
        assert_eq!(p.option_value("out", "default"), "result.txt");
        assert_eq!(p.arguments, vec!["input.sol"]);
    }
}
