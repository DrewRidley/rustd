# Stress edits

88 of 88 steps right, 0 restarts. Log: /tmp/rustd-stress-out-g4x9k5eh/daemon.log

| # | Edit | Reply | ms | Result |
|---|---|---|---:|---|
| 0 | body: constant in a leaf function | patch | 52 | same |
| 1 | body: in main | patch | 54 | same |
| 2 | body: generic function used from another crate | patch | 51 | same |
| 3 | body: generic method of a generic struct | patch | 54 | same |
| 4 | body: trait default method | patch | 39 | same |
| 5 | body: closure inside a function | patch | 56 | same |
| 6 | body: async fn | patch | 53 | same |
| 7 | body: async fn gets another await | patch | 72 | same |
| 8 | body: Drop impl | patch | 48 | same |
| 9 | body: match arm in trait impl | patch | 53 | same |
| 10 | blank line at top of a file with line!() and track_caller | patch | 39 | same |
| 11 | blank line above a panic site | patch | 52 | same |
| 12 | comment appended | patch | 37 | same |
| 13 | new private function, unused | patch | 44 | same |
| 14 | new public function, called from main | patch | 90 | same |
| 15 | function renamed, callers updated | patch | 63 | same |
| 16 | function removed | patch | 42 | same |
| 17 | function moved to another file of the crate | patch | 79 | same |
| 18 | signature: parameter added, callers in another crate updated | patch | 91 | same |
| 19 | signature: return type changed | patch | 65 | same |
| 20 | signature: parameter type &str to String | patch | 72 | same |
| 21 | signature: function made generic | full | 116 | same |
| 22 | signature: impl Fn return becomes Box<dyn Fn> | patch | 66 | same |
| 23 | signature: const fn body and a const that uses it | patch | 54 | same |
| 24 | signature: fn becomes async, caller awaits through block_on | patch | 88 | same |
| 25 | struct: field added, constructor updated | patch | 94 | same |
| 26 | struct: fields reordered (derived PartialOrd changes) | patch | 125 | same |
| 27 | struct: field type widened across crates | patch | 73 | same |
| 28 | struct: private field added to a generic struct | patch | 81 | same |
| 29 | enum: variant added, matches updated | patch | 69 | same |
| 30 | enum: variants reordered (discriminants move) | patch | 58 | same |
| 31 | derive added | patch | 74 | same |
| 32 | derive removed, hand-written impl instead | full | 100 | same |
| 33 | repr and type alias | patch | 72 | same |
| 34 | trait: method added with a default (vtable grows) | patch | 102 | same |
| 35 | trait: default overridden in one impl | patch | 48 | same |
| 36 | trait: required method added, all impls updated | patch | 82 | same |
| 37 | trait: associated const changed | patch | 43 | same |
| 38 | trait: new impl for a foreign-crate type | patch | 69 | same |
| 39 | trait: impl removed | patch | 92 | same |
| 40 | trait: new trait with blanket impl | patch | 93 | same |
| 41 | trait: generic bound tightened | patch | 124 | same |
| 42 | trait: associated type changed on an Iterator impl | patch | 217 | same |
| 43 | trait: operator impl added | patch | 125 | same |
| 44 | generic: new instantiation of an existing generic | patch | 131 | same |
| 45 | generic: std collection never used before | patch | 245 | same |
| 46 | generic: type parameter added to a function | patch | 82 | same |
| 47 | generic: const generic introduced | patch | 90 | same |
| 48 | macro: body of macro_rules used in another crate | patch | 57 | same |
| 49 | macro: function-making macro changed | patch | 50 | same |
| 50 | macro: new invocation makes a new function | patch | 76 | same |
| 51 | macro: new macro defined and used in the same edit | patch | 98 | same |
| 52 | macro: macro that declares a struct and impl | patch | 93 | same |
| 53 | macro: format string changed | patch | 49 | same |
| 54 | macro: matches!, vec!, assert!, write! in a new function | patch | 177 | same |
| 55 | const value changed (used in a const fn and an array length) | patch | 79 | same |
| 56 | static value changed | full | 87 | same |
| 57 | static added and used | patch | 67 | same |
| 58 | static initial value of an atomic changed | full | 67 | same |
| 59 | thread_local added | patch | 78 | same |
| 60 | static with a lazily made value | patch | 87 | same |
| 61 | new file and module | patch | 69 | same |
| 62 | edit to the file just added | patch | 35 | same |
| 63 | module renamed (file moved) | patch | 126 | same |
| 64 | module becomes a directory with a submodule | patch | 69 | same |
| 65 | re-export added and used | patch | 72 | same |
| 66 | inline module with nested function and impl | patch | 72 | same |
| 67 | include_str! of a new file | patch | 97 | same |
| 68 | the included file changes | patch | 58 | same |
| 69 | module removed | patch | 74 | same |
| 70 | #[inline(never)] and #[cold] added | patch | 39 | same |
| 71 | cfg(debug_assertions) branch | patch | 87 | same |
| 72 | visibility narrowed and widened | patch | 43 | same |
| 73 | BROKEN: syntax error | error | 358 | survived (error: /tmp/rustd-stress-0hjirdq1/mid/src/text.rs:17: expect) |
| 74 | fixed again, with a body change | full | 82 | same |
| 75 | BROKEN: type error in a body | error | 110 | survived (error: `main` is not compiled by rustd and rustc's copy is f) |
| 76 | BROKEN: still broken, another file edited too | error | 6 | survived (error: `main` is not compiled by rustd and rustc's copy is f) |
| 77 | fixed again | full | 71 | same |
| 78 | BROKEN: unresolved name in main | error | 25 | survived (error: `main` has errors, `cargo check` shows them: frontend) |
| 79 | fixed again by adding the missing function | full | 104 | same |
| 80 | BROKEN: caller not updated after a signature change | error | 40 | survived (error: `main` has errors, `cargo check` shows them: frontend) |
| 81 | fixed again: caller updated | full | 115 | same |
| 82 | BROKEN: file emptied | error | 25 | survived (error: `main` has errors, `cargo check` shows them: frontend) |
| 83 | file restored and three crates edited in one build | full | 116 | same |
| 84 | whole file rewritten in another order | patch | 37 | same |
| 85 | large generated function added | patch | 212 | same |
| 86 | everything back to the first source | full | 112 | same |
| 87 | and forward again to the last source | full | 304 | same |
