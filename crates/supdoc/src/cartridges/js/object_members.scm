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
