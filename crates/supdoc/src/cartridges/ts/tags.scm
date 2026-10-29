; TypeScript symbols (03 R4). Written in tree-sitter's tags.scm conventions. Unlike the grammar's
; bundled tags.scm, this covers fields, enum members, namespaces, type aliases and top-level
; `const` bindings. Nesting by containment gives `Class.method` and `Namespace.member`.

; Declarations

(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(abstract_class_declaration name: (type_identifier) @name) @definition.class
(interface_declaration name: (type_identifier) @name) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.class
(internal_module name: [(identifier) (nested_identifier)] @name) @definition.module

; Top-level bindings, with or without `export`. Locals inside functions are not symbols.

(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name) @definition.constant))
(program
  (variable_declaration
    (variable_declarator name: (identifier) @name) @definition.constant))
(program
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator name: (identifier) @name) @definition.constant)))
(program
  (export_statement
    declaration: (variable_declaration
      (variable_declarator name: (identifier) @name) @definition.constant)))
(internal_module
  body: (statement_block
    (lexical_declaration
      (variable_declarator name: (identifier) @name) @definition.constant)))
(internal_module
  body: (statement_block
    (export_statement
      declaration: (lexical_declaration
        (variable_declarator name: (identifier) @name) @definition.constant))))

; Members

(method_definition name: [(property_identifier) (private_property_identifier)] @name) @definition.method
(abstract_method_signature name: [(property_identifier) (private_property_identifier)] @name) @definition.method
(method_signature name: [(property_identifier) (private_property_identifier)] @name) @definition.method
(public_field_definition name: [(property_identifier) (private_property_identifier)] @name) @definition.field
(property_signature name: (property_identifier) @name) @definition.field
(enum_body name: (property_identifier) @name @definition.constant)
(enum_assignment name: (property_identifier) @name) @definition.constant
