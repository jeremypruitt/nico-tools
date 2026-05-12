#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdType {
    Workflow,
    Host,
    Dpu,
    Request,
}

impl IdType {
    pub fn label_key(&self) -> &'static str {
        match self {
            IdType::Workflow => "workflow_id",
            IdType::Host => "host_id",
            IdType::Dpu => "dpu_id",
            IdType::Request => "request_id",
        }
    }

    pub fn cli_name(&self) -> &'static str {
        match self {
            IdType::Workflow => "workflow",
            IdType::Host => "host",
            IdType::Dpu => "dpu",
            IdType::Request => "request",
        }
    }

    pub fn from_cli_name(s: &str) -> Option<Self> {
        match s {
            "workflow" => Some(IdType::Workflow),
            "host" => Some(IdType::Host),
            "dpu" => Some(IdType::Dpu),
            "request" => Some(IdType::Request),
            _ => None,
        }
    }
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
    // Bare carbide-style machine IDs: long, no prefix, only [a-z0-9_-]
    // (Crockford-base32 ULID-derived plus separators). machines.id is
    // varchar(64) and observed values are in the ~26-58 char range.
    // Both Host and DPU resolve to a `machines` row, so default to Host.
    if id.len() >= 26 && id.chars().all(is_machine_id_char) {
        return Some(IdType::Host);
    }
    None
}

fn is_machine_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
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

    #[test]
    fn bare_carbide_machine_id_detects_as_host() {
        // Carbide stores both Hosts and DPUs in `machines.id` as bare
        // ~58-char IDs with no prefix. Default detection should treat
        // them as Host (the postgres source maps Host and Dpu to the
        // same `machines` row anyway). Operators can still pin it to
        // Dpu via `--type dpu`.
        let id = "01HXP1ABCDEFGHJKMNPQRSTVWXYZ0123456789ABCDEFGHJKMNPQRSTVWX";
        assert_eq!(detect_id_type(id), Some(IdType::Host));
    }

    #[test]
    fn long_unknown_string_with_special_chars_is_not_host() {
        assert_eq!(detect_id_type("not a machine id, has spaces"), None);
        assert_eq!(detect_id_type("01HXP1!@#$%^&*()not_an_id_x_x_x_x_x_x_x"), None);
    }

    #[test]
    fn short_unprefixed_string_is_not_detected() {
        assert_eq!(detect_id_type("short"), None);
        assert_eq!(detect_id_type("01HXP1ABC"), None);
    }

    #[test]
    fn label_keys() {
        assert_eq!(IdType::Workflow.label_key(), "workflow_id");
        assert_eq!(IdType::Host.label_key(), "host_id");
        assert_eq!(IdType::Dpu.label_key(), "dpu_id");
        assert_eq!(IdType::Request.label_key(), "request_id");
    }

    #[test]
    fn cli_names() {
        assert_eq!(IdType::Workflow.cli_name(), "workflow");
        assert_eq!(IdType::Host.cli_name(), "host");
        assert_eq!(IdType::Dpu.cli_name(), "dpu");
        assert_eq!(IdType::Request.cli_name(), "request");
    }

    #[test]
    fn from_cli_name_roundtrips() {
        for variant in [IdType::Workflow, IdType::Host, IdType::Dpu, IdType::Request] {
            assert_eq!(IdType::from_cli_name(variant.cli_name()), Some(variant));
        }
        assert_eq!(IdType::from_cli_name("unknown"), None);
        assert_eq!(IdType::from_cli_name(""), None);
    }
}
