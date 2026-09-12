#[path = "sql/determinism.rs"]
mod determinism;
#[path = "sql/ident_refusals.rs"]
mod ident_refusals;
#[path = "sql/joins.rs"]
mod joins;
#[path = "sql/no_sql_text_escape_hatch.rs"]
mod no_sql_text_escape_hatch;
#[path = "sql/null_is_a_node.rs"]
mod null_is_a_node;
#[path = "sql/parameters_never_carry_values.rs"]
mod parameters_never_carry_values;
#[path = "sql/plan_invariants.rs"]
mod plan_invariants;
#[path = "sql/predicate_depth.rs"]
mod predicate_depth;
#[path = "sql/projection_rules.rs"]
mod projection_rules;
#[path = "sql/public_contract.rs"]
mod public_contract;
#[path = "sql/search_family.rs"]
mod search_family;
#[path = "sql/write_family.rs"]
mod write_family;
#[path = "sql/upsert_compiler.rs"]
mod upsert_compiler;

trait NumberedParameters {
    fn placeholder_count(&self) -> usize;
    fn placeholder_slots(&self) -> Vec<usize>;
}

impl NumberedParameters for zeroship_data_orm::sql::compiler::CompiledQuery {
    fn placeholder_count(&self) -> usize {
        let bytes = self.sql().as_bytes();
        bytes
            .iter()
            .enumerate()
            .filter(|(i, b)| **b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
            .count()
    }

    fn placeholder_slots(&self) -> Vec<usize> {
        let bytes = self.sql().as_bytes();
        let mut slots: Vec<usize> = Vec::new();
        let mut index = 0_usize;
        while index < bytes.len() {
            if bytes[index] != b'$' {
                index += 1;
                continue;
            }
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end == start {
                index += 1;
                continue;
            }
            if let Ok(slot) = self.sql()[start..end].parse::<usize>() {
                slots.push(slot);
            }
            index = end;
        }
        slots.sort_unstable();
        slots.dedup();
        slots
    }}
