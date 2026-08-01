/// Per-agent turn submission (#175 M2). `agent.send` writes literal bytes
/// and stays frozen (P5); a mailbox that delivers a message as a chat turn
/// must know how each agent's CLI *submits* — otherwise it's a paste
/// buffer. Text encoding (bracketed paste) is decided by the live pane
/// runtime, not here.
pub struct SubmitPlan {
    /// Bytes sent after the text to submit the turn.
    pub submit_bytes: Vec<u8>,
    /// True when the agent has no verified recipe and got the generic
    /// text+Enter path — surfaced as a warning, never silent (mirrors F2's
    /// loud-refusal posture).
    pub generic: bool,
}

/// Agents whose interactive CLIs are verified to submit a typed line on a
/// plain carriage return.
const VERIFIED_ENTER_AGENTS: [&str; 3] = ["claude", "codex", "opencode"];

pub fn submit_plan(agent_label: &str) -> SubmitPlan {
    let generic = !VERIFIED_ENTER_AGENTS.contains(&agent_label);
    SubmitPlan {
        submit_bytes: b"\r".to_vec(),
        generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_agents_get_a_non_generic_enter_recipe() {
        for agent in ["claude", "codex", "opencode"] {
            let plan = submit_plan(agent);
            assert_eq!(plan.submit_bytes, b"\r");
            assert!(!plan.generic, "{agent} is verified");
        }
    }

    #[test]
    fn unknown_agents_fall_back_generic_and_say_so() {
        let plan = submit_plan("qodercli");
        assert_eq!(plan.submit_bytes, b"\r");
        assert!(plan.generic, "unverified recipe must be flagged");
    }
}
