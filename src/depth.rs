//! The fork-bomb guard. Every copy tau starts through bash gets TAU_DEPTH one
//! higher than its parent's, and a copy deeper than TAU_MAX_DEPTH refuses to
//! start.

pub const DEFAULT_MAX_DEPTH: u32 = 3;

#[derive(Debug, Clone, Copy)]
pub struct Depth {
    pub depth: u32,
    pub max: u32,
}

fn var(k: &str) -> Option<u32> {
    std::env::var(k).ok().and_then(|v| v.trim().parse().ok())
}

impl Depth {
    pub fn from_env() -> Depth {
        Depth {
            depth: var("TAU_DEPTH").unwrap_or(0),
            max: var("TAU_MAX_DEPTH").unwrap_or(DEFAULT_MAX_DEPTH),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.depth > self.max {
            return Err(format!(
                "too deep: this copy is depth {} and TAU_MAX_DEPTH is {}",
                self.depth, self.max
            ));
        }
        Ok(())
    }

    /// Environment a child copy starts with.
    pub fn child_env(&self) -> Vec<(String, String)> {
        vec![
            ("TAU_DEPTH".into(), (self.depth + 1).to_string()),
            ("TAU_MAX_DEPTH".into(), self.max.to_string()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_go_one_deeper_and_stop_at_the_limit() {
        let d = Depth { depth: 2, max: 3 };
        let env: std::collections::HashMap<_, _> = d.child_env().into_iter().collect();
        assert_eq!(env["TAU_DEPTH"], "3");
        assert_eq!(env["TAU_MAX_DEPTH"], "3");
        assert!(d.check().is_ok());
        assert!(Depth { depth: 3, max: 3 }.check().is_ok());
        assert!(
            Depth { depth: 4, max: 3 }
                .check()
                .unwrap_err()
                .contains("too deep")
        );
    }
}
