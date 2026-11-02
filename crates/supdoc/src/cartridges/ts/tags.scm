; TypeScript and TSX symbols, one query for both cartridges: the TSX grammar has every TypeScript
; node kind. Written in tree-sitter's tags.scm conventions. Unlike the grammar's bundled
; tags.scm, this covers fields, enum members, namespaces, type aliases and top-level
; bindings. Nesting by containment gives `Class.method` and `Namespace.member`.
; Overloads and get/set pairs share one name and fold into one symbol spanning all of them.
; Members of an object literal come from `js/object_members.scm`, appended by `ts/mod.rs`.

; Declarations

(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(function_signature name: (identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(abstract_class_declaration name: (type_identifier) @name) @definition.class
(interface_declaration name: (type_identifier) @name) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.class

; A namespace is named by its dotted path: `namespace Outer.Inner` is `Outer.Inner`, and its
; members are `Outer.Inner.X`. The path is read as scope segments up to three deep. A deeper path
; is a nameless scope, so what is inside it is not a symbol rather than a wrongly named one.

(internal_module name: (identifier) @name) @definition.module
(internal_module
  name: (nested_identifier
    object: (identifier) @supdoc.scope.name
    property: (property_identifier) @name)) @definition.module
(internal_module
  name: (nested_identifier
    object: (member_expression
      object: (identifier) @supdoc.scope.name
      property: (property_identifier) @supdoc.scope.qualifier)
    property: (property_identifier) @name)) @definition.module
(internal_module
  name: (nested_identifier
    object: (member_expression
      object: (member_expression)))) @supdoc.scope

; Bindings, with or without `export` and `declare`, at the top of a file or of a namespace.
; Locals inside functions are not symbols. `export const Button = () => <div />` is the usual
; way to write a component, and `export declare const VERSION: string` documents a library.

(program
  [
    (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)
    (variable_declaration (variable_declarator name: (identifier) @name) @definition.constant)
    (ambient_declaration
      [
        (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)
        (variable_declaration (variable_declarator name: (identifier) @name) @definition.constant)
      ])
    (export_statement
      declaration: [
        (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)
        (variable_declaration (variable_declarator name: (identifier) @name) @definition.constant)
        (ambient_declaration
          [
            (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)
            (variable_declaration (variable_declarator name: (identifier) @name) @definition.constant)
          ])
      ])
  ])
(internal_module
  body: (statement_block
    [
      (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)
      (variable_declaration (variable_declarator name: (identifier) @name) @definition.constant)
      (export_statement
        declaration: [
          (lexical_declaration (variable_declarator name: (identifier) @name) @definition.constant)
          (variable_declaration (variable_declarator name: (identifier) @name) @definition.constant)
        ])
    ]))

; Members

(class_body
  (method_definition name: [(property_identifier) (private_property_identifier)] @name) @definition.method)
(abstract_method_signature name: [(property_identifier) (private_property_identifier)] @name) @definition.method
(method_signature name: [(property_identifier) (private_property_identifier)] @name) @definition.method
(public_field_definition name: [(property_identifier) (private_property_identifier)] @name) @definition.field
(property_signature name: (property_identifier) @name) @definition.field
(enum_body name: (property_identifier) @name @definition.constant)
(enum_assignment name: (property_identifier) @name) @definition.constant
