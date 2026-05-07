/// Run the ops dashboard. Currently a placeholder; returns exit code 3
/// after printing a "not yet" notice to stderr.
pub fn run_ops() -> i32 {
    eprintln!("not yet");
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ops_returns_exit_code_three() {
        assert_eq!(run_ops(), 3);
    }
}
