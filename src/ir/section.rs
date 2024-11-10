//! Enums the represent a section of a Module or a Component

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
/// Represents a Section in a Component
pub enum ComponentSection {
    Module,
    Alias,
    CoreType,
    ComponentType,
    ComponentImport,
    ComponentExport,
    CoreInstance,
    ComponentInstance,
    Canon,
    CustomSection,
    Component,
    ComponentStartSection,
}
