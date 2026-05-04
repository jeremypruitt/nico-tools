#[derive(Debug, PartialEq)]
pub enum IdType {
    Workflow,
    Host,
    Dpu,
    Request,
}

pub fn detect_id_type(id: &str) -> Option<IdType> {
    if id.starts_with("hp-") || id.starts_with("wf-") {
        return Some(IdType::Workflow);
    }
    if id.starts_with("host-") {
        return Some(IdType::Host);
    }
    if id.starts_with("dpu-") {
        return Some(IdType::Dpu);
    }
    if id.starts_with("req-") {
        return Some(IdType::Request);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_heuristics() {
        let cases = [
            ("hp-7f3a2c",       Some(IdType::Workflow)),
            ("wf-abc123",       Some(IdType::Workflow)),
            ("host-r12u5",      Some(IdType::Host)),
            ("host-prov-r12u5", Some(IdType::Host)),
            ("dpu-bf3-r12u5",   Some(IdType::Dpu)),
            ("req-a83b",        Some(IdType::Request)),
            ("unknown-xyz",     None),
            ("",                None),
        ];
        for (input, expected) in cases {
            assert_eq!(detect_id_type(input), expected, "input: {input:?}");
        }
    }
}
