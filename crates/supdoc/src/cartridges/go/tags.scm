; Go symbols (03 R4, R5a). Written in tree-sitter's tags.scm conventions plus the
; @supdoc.scope.name extension. Unlike the grammar's bundled tags.scm, this covers struct
; fields, interface methods, constants and variables, and names methods by their receiver.
; Package-level names only: a local `var` or `const` inside a function is not addressable.

; Functions

(function_declaration name: (identifier) @name) @definition.function

; A method is named by its receiver type, with pointers and type arguments stripped:
; `func (c *Config[T]) Validate()` is `Config.Validate`.

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (type_identifier) @supdoc.scope.name
        (pointer_type (type_identifier) @supdoc.scope.name)
        (generic_type type: (type_identifier) @supdoc.scope.name)
        (pointer_type (generic_type type: (type_identifier) @supdoc.scope.name))
      ]))
  name: (field_identifier) @name) @definition.method

; Types

(type_spec name: (type_identifier) @name) @definition.class
(type_alias name: (type_identifier) @name) @definition.type

; Members

(field_declaration name: (field_identifier) @name) @definition.field
(method_elem name: (field_identifier) @name) @definition.method

; Constants and variables

(source_file
  (const_declaration (const_spec name: (identifier) @name) @definition.constant))
(source_file
  (var_declaration (var_spec name: (identifier) @name) @definition.constant))
(source_file
  (var_declaration (var_spec_list (var_spec name: (identifier) @name) @definition.constant)))
