; Members of an object literal bound at the top of a module by `const`, `let` or `var`, exported
; or not: `api.get`, `api.timeout`. Shared by the JavaScript and TypeScript cartridges.

(program
  [
    (lexical_declaration (variable_declarator value: (object (method_definition name: (property_identifier) @name) @definition.method)))
    (variable_declaration (variable_declarator value: (object (method_definition name: (property_identifier) @name) @definition.method)))
    (export_statement
      declaration: [
        (lexical_declaration (variable_declarator value: (object (method_definition name: (property_identifier) @name) @definition.method)))
        (variable_declaration (variable_declarator value: (object (method_definition name: (property_identifier) @name) @definition.method)))
      ])
  ])
(program
  [
    (lexical_declaration (variable_declarator value: (object (pair key: (property_identifier) @name) @definition.field)))
    (variable_declaration (variable_declarator value: (object (pair key: (property_identifier) @name) @definition.field)))
    (export_statement
      declaration: [
        (lexical_declaration (variable_declarator value: (object (pair key: (property_identifier) @name) @definition.field)))
        (variable_declaration (variable_declarator value: (object (pair key: (property_identifier) @name) @definition.field)))
      ])
  ])

; Members of a default-exported object, `export default { ... }`, or of the object that is the
; first argument of the call exported by default, `export default defineConfig({ ... })`. The
; `default` keyword names the segment, so a member is `default.lint` and never collides with a
; top-level binding called `lint`.
(program
  (export_statement
    "default" @supdoc.scope.name
    value: [
      (object (method_definition name: (property_identifier) @name) @definition.method)
      (call_expression arguments: (arguments . (object (method_definition name: (property_identifier) @name) @definition.method)))
    ]))
(program
  (export_statement
    "default" @supdoc.scope.name
    value: [
      (object (pair key: (property_identifier) @name) @definition.field)
      (call_expression arguments: (arguments . (object (pair key: (property_identifier) @name) @definition.field)))
    ]))

; One level of nesting under a default-exported object: `default.a.b`.
(program
  (export_statement
    "default"
    value: [
      (object (pair value: (object (pair key: (property_identifier) @name) @definition.field)))
      (call_expression arguments: (arguments . (object (pair value: (object (pair key: (property_identifier) @name) @definition.field)))))
    ]))
