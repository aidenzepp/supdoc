; Rust symbols (03 R4, R5). Written in tree-sitter's tags.scm conventions plus the
; @supdoc.scope extensions. Unlike the grammar's bundled tags.scm, this covers fields, enum
; variants, consts, statics, associated types and generic or trait impls.

; Items

(function_item name: (identifier) @name) @definition.function
(function_signature_item name: (identifier) @name) @definition.function
(struct_item name: (type_identifier) @name) @definition.class
(enum_item name: (type_identifier) @name) @definition.class
(union_item name: (type_identifier) @name) @definition.class
(trait_item name: (type_identifier) @name) @definition.interface
(type_item name: (type_identifier) @name) @definition.type
(associated_type name: (type_identifier) @name) @definition.type
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(mod_item name: (identifier) @name) @definition.module
(macro_definition name: (identifier) @name) @definition.macro

; Members

(field_declaration name: (field_identifier) @name) @definition.field
(enum_variant name: (identifier) @name) @definition.constant

; impl blocks name their methods by type, with generics stripped: `RingBuffer.new`.
; The type arguments are captured so `impl Foo<u8>` also answers to `Foo<u8>.new`.

(impl_item
  type: [
    (type_identifier) @supdoc.scope.name
    (scoped_type_identifier name: (type_identifier) @supdoc.scope.name)
    (reference_type type: (type_identifier) @supdoc.scope.name)
    (reference_type type: (scoped_type_identifier name: (type_identifier) @supdoc.scope.name))
  ]) @supdoc.scope

(impl_item
  type: (generic_type
    type: [
      (type_identifier) @supdoc.scope.name
      (scoped_type_identifier name: (type_identifier) @supdoc.scope.name)
    ]
    type_arguments: (type_arguments) @supdoc.scope.arguments)) @supdoc.scope

(impl_item
  type: (reference_type
    type: (generic_type
      type: [
        (type_identifier) @supdoc.scope.name
        (scoped_type_identifier name: (type_identifier) @supdoc.scope.name)
      ]
      type_arguments: (type_arguments) @supdoc.scope.arguments))) @supdoc.scope

; A trait impl's methods must name the trait: `Config.Display.fmt`.

(impl_item
  trait: [
    (type_identifier) @supdoc.scope.qualifier
    (scoped_type_identifier name: (type_identifier) @supdoc.scope.qualifier)
  ]) @supdoc.scope

(impl_item
  trait: (generic_type
    type: [
      (type_identifier) @supdoc.scope.qualifier
      (scoped_type_identifier name: (type_identifier) @supdoc.scope.qualifier)
    ]
    type_arguments: (type_arguments) @supdoc.scope.qualifier.arguments)) @supdoc.scope

; Any other impl (`impl Trait for [T]`, `for (A, B)`) is still a scope, but a nameless one, so its
; methods aren't symbols rather than being mistaken for free functions.

(impl_item) @supdoc.scope
