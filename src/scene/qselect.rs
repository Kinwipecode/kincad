// Quick Select filter types. Shared between the scene's candidate search
// (scene/selection.rs) and whatever UI presents the filter; kept out of the
// app layer so the render core stays self-contained.

/// Operator the Quick Select filter applies between an entity's
/// property value and the user-typed test value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QSelectOp {
    /// `*Any value` — the entity matches as long as the type filter
    /// passes; the value column is ignored.
    Any,
    Eq,
    Neq,
    Gt,
    Lt,
}

/// Candidate set searched by Quick Select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QSelectScope {
    CurrentSpace,
    CurrentSelection,
}

impl std::fmt::Display for QSelectScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match self {
            QSelectScope::CurrentSpace => "Current space",
            QSelectScope::CurrentSelection => "Current selection",
        };
        f.write_str(crate::t!(source).as_ref())
    }
}

/// Whether matching candidates are kept or removed from the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QSelectMode {
    Include,
    Exclude,
}

/// Editor used by the Quick Select value field.
#[derive(Clone, Debug)]
pub enum QSelectValueEditor {
    Text,
    Number,
    Choice(Vec<String>),
}

impl std::fmt::Display for QSelectOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            QSelectOp::Any => "* Any value",
            QSelectOp::Eq => "= Equals",
            QSelectOp::Neq => "!= Not equal",
            QSelectOp::Gt => "> Greater than",
            QSelectOp::Lt => "< Less than",
        };
        f.write_str(crate::t!(s).as_ref())
    }
}

/// One row in the Quick Select Properties pick_list. `field` is the
/// stable identifier (`"layer"`, `"start_x"`, …) used to look up the
/// value on each candidate entity; `label` is the human label rendered
/// in the dropdown. Equality only compares `field` so the pick_list
/// round-trips selection correctly even when labels are duplicated.
#[derive(Clone, Debug)]
pub struct QSelectPropertyChoice {
    pub field: String,
    pub label: String,
    pub editor: QSelectValueEditor,
}

impl PartialEq for QSelectPropertyChoice {
    fn eq(&self, other: &Self) -> bool {
        self.field == other.field
    }
}

impl Eq for QSelectPropertyChoice {}

impl std::fmt::Display for QSelectPropertyChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}
