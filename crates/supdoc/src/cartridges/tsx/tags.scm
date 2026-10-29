; TSX symbols (03 R4, R5a). Written in tree-sitter's tags.scm conventions. Unlike the grammar's
; bundled tags.scm, this covers classes, fields, enums and their members, type aliases and
; module-level variables, and leaves out references. Names nest by containment: `Counter.increment`.
; Overloads and get/set pairs share one name, so resolving them reports every candidate (R5a).

; Declarations

(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(abstract_class_declaration name: (type_identifier) @name) @definition.class
(interface_declaration name: (type_identifier) @name) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.class
(internal_module name: (identifier) @name) @definition.module

; Members

(method_definition name: [(property_identifier) (private_property_identifier)] @name) @definition.method
(abstract_method_signature name: (property_identifier) @name) @definition.method
(method_signature name: (property_identifier) @name) @definition.method
(public_field_definition name: [(property_identifier) (private_property_identifier)] @name) @definition.field
(property_signature name: (property_identifier) @name) @definition.field
(enum_body name: (property_identifier) @name @definition.constant)
(enum_assignment name: (property_identifier) @name) @definition.constant

; Variables are symbols only at the top of a file or of a namespace, never inside a function.
; `export const Button = () => <div />` is the usual way to write a component.

(program
  (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant))
(program
  (variable_declaration (variable_declarator name: (identifier) @name) @definition.constant))
(program
  (export_statement
    declaration: (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)))
(internal_module
  body: (statement_block
    (export_statement
      declaration: (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant))))
