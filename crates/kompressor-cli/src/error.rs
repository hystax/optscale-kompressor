//! What a command can fail with, and what the shell learns from it.
//!
//! The exit code is part of the interface: `bench replay --compare` is meant to
//! run in CI, where "the numbers got worse" and "the corpus file is missing"
//! have to be told apart without parsing the output. So a regression has a code
//! of its own, and so does a bad invocation.

use std::fmt;

/// Failure, classified by what the caller should do about it.
#[derive(Debug)]
pub enum CliError {
    /// Something went wrong while doing the work: a file that would not open, a
    /// database that would not answer, a handle that does not resolve.
    Runtime(String),
    /// The command or the configuration was wrong. Nothing was attempted.
    Usage(String),
    /// A benchmark comparison found a regression. The run itself succeeded.
    Regressed,
    /// The wrapped agent ended with this status, which becomes ours: `wrap` is
    /// transparent, so `kompressor wrap -- make test` in CI has to fail the way
    /// `make test` failed. Nothing is printed for it — the agent already said
    /// whatever it had to say.
    AgentExit(i32),
    /// The command already reported what it found and ends with this code:
    /// `doctor`'s 1 for warnings and 2 for failures. Nothing more is printed.
    Quiet(i32),
}

impl CliError {
    /// Process exit code: 0 success, 1 runtime, 2 usage or config, 3 regression.
    pub fn code(&self) -> i32 {
        match self {
            CliError::Runtime(_) => 1,
            CliError::Usage(_) => 2,
            CliError::Regressed => 3,
            CliError::Quiet(code) => *code,
            // Kept inside what a shell can report; 0 would turn a failure into
            // success.
            CliError::AgentExit(code) => match code & 0xff {
                0 => 1,
                code => code,
            },
        }
    }

    /// A runtime failure from anything that renders itself.
    pub fn runtime(context: &str, error: impl fmt::Display) -> CliError {
        CliError::Runtime(format!("{context}: {error}"))
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Runtime(message) | CliError::Usage(message) => f.write_str(message),
            // The detail is in the comparison table already printed; repeating
            // it on stderr would bury the table it refers to.
            CliError::Regressed => f.write_str("benchmark comparison regressed"),
            CliError::AgentExit(code) => write!(f, "the agent exited with status {code}"),
            CliError::Quiet(code) => write!(f, "exit status {code}"),
        }
    }
}

pub type CliResult = Result<(), CliError>;
