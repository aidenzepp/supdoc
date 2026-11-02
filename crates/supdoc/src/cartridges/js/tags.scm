; JavaScript symbols. Written in tree-sitter's tags.scm conventions. Unlike the
; grammar's bundled tags.scm, this covers class fields and top-level bindings, and it leaves out
; call and `new` references, which aren't definitions.

; Functions and classes

(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(class_declaration name: (identifier) @name) @definition.class

; Class members: `Counter.increment`, `Counter.#count`, `Counter.step`

(class_body
  (method_definition name: (property_identifier) @name) @definition.method)
(class_body
  (method_definition name: (private_property_identifier) @name) @definition.method)
(class_body
  (field_definition property: (property_identifier) @name) @definition.field)
(class_body
  (field_definition property: (private_property_identifier) @name) @definition.field)

; Bindings declared at the top of a module, exported or not. Bindings inside functions are
; implementation detail, so they aren't addressable.

(program
  [
    (lexical_declaration (variable_declarator name: (identifier) @name) @definition.variable)
    (variable_declaration (variable_declarator name: (identifier) @name) @definition.variable)
    (export_statement
      declaration: [
        (lexical_declaration (variable_declarator name: (identifier) @name) @definition.variable)
        (variable_declaration (variable_declarator name: (identifier) @name) @definition.variable)
      ])
  ])
