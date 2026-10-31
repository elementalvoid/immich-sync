//! The one way the app writes an accessibility name.
//!
//! gpui builds a node only for an element that declares a role: its element
//! pushes one behind `if let Some(role) = self.element.a11y_role()`, and a plain
//! `div()` reports `GenericContainer`, which is filtered to `None`. So an
//! `aria_label` or an `aria_description` on a bare element is stored and read by
//! nobody, and the test snapshots read the declaration either way, so no test
//! can tell the difference (#74-#82).
//!
//! [`named`] is the role and the name together, so the two cannot drift apart.

use gpui_kit::base::ObservedElement;
use gpui_kit::*;

/// `name`, and `description` when the element has one, on an element whose
/// `role` is what makes gpui build the node they live on.
pub(crate) fn named(
    role: Role,
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    description: Option<SharedString>,
) -> ObservedElement<Stateful<Div>> {
    let element = div().id(id).test_support().role(role).aria_label(name);
    match description {
        Some(description) => element.aria_description(description),
        None => element,
    }
}
