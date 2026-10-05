# Steel Scheme language reference

The `code` you send to `scheme_eval` is [Steel](https://github.com/mattwparas/steel)
Scheme. This page covers the *language*; `blender-mcp://reference/scheme` covers the
Blender bindings and `blender-mcp://reference/blender-api` covers how Blender's Python
API maps onto them.

Everything below was verified against the running engine rather than assumed.

## The environment persists

Definitions survive between `scheme_eval` calls, so build a toolkit once and use it in
later calls:

```scheme
(define (deg->rad d) (* d 0.017453292519943295))
```

Pass `reset: true` to `scheme_eval` to get a clean environment back.

**Redefining your own names updates their callers.** A top-level `define` of a name
you defined earlier is compiled as `set!`, so every function that uses it -- including
functions from earlier calls -- sees the new definition. `set!` works the same way:

```scheme
(define speed 1) (define (step) (* speed 2))   ; one call
(define speed 5)                                ; a later call (or (set! speed 5))
(step)                                          ; => 10
(define (step) 0)                               ; replaces step for its callers too
```

Wrapping a function works in one call: the old value is read before it is replaced.

```scheme
(define old-step step)
(define (step) (+ (old-step) 1))
```

Builtin and stdlib names are the exception: `(define (cos x) ...)` makes a new binding
that code defined earlier, including the stdlib, does not see, and the reply carries a
`warnings` entry saying so. Choose another name instead.

**`void` is a value, not a function.** Write `void`, not `(void)`, for Blender's `None`
(for example `(rna-set! child "parent" void)`).

**Load toolkits with `(use "name")`.** When the server is started with
`--scheme-library DIR`, a top-level `(use "crossing-toolkit")` loads
`DIR/crossing-toolkit.scm` in place and returns `"loaded crossing-toolkit (N forms)"`.
Using it again after editing the file updates existing callers, so a toolkit survives a
reset or restart with one line. Library files pass the same sandbox checks as your code.

Every top-level expression contributes a value; several expressions return a list of
them. `define` evaluates to `#<void>`, so a script of definitions returns a row of
voids — harmless, but wrap the interesting value last if you want a clean result.

## Special forms

`define`, `lambda`, `let`, `let*`, `letrec`, named `let`, `do`, `if`, `cond`, `case`,
`when`, `unless`, `begin`, `set!`, `quote`/`quasiquote`/`unquote`, `and`, `or`, `not`,
and `define-syntax` with `syntax-rules`.

```scheme
(let loop ([i 0] [acc '()])
  (if (= i 5) (reverse acc) (loop (+ i 1) (cons i acc))))
```

`set!` returns the *previous* value, not the new one.

## Two names you must not shadow

Steel's prelude is not shadowable, and the errors do not say so. These cost real
debugging time:

- **`fn` is an alias for `lambda`.** Defining anything named `fn` fails with a message
  that never mentions `fn`:

  ```scheme
  (define (fn a) a)
  ; => Parse: Syntax Error: lambda expected at least 2 arguments
  ```

  The form is read as a `lambda`, so the parser complains about the lambda's shape.

- **`log` is the natural logarithm.** `(set! log ...)` fails with `cannot mutate
  module-required identifier`.

The same applies to any other prelude binding. When a name behaves impossibly, rename it
before investigating anything else.

## Errors

`call-with-exception-handler` takes the **handler first** and the thunk second. It
catches bridge errors too, which is what makes optional probing possible:

```scheme
(define (try-get reference attribute fallback)
  (call-with-exception-handler
    (lambda (e) fallback)
    (lambda () (rna-get reference attribute))))
```

`error-object-message` extracts the text, and `error-object?` tests a caught value.
Raise your own with `(error "message")`.

Without a handler the first failure aborts the whole `scheme_eval`, discarding the
results of every expression after it — so wrap anything speculative.

`call/cc` and `dynamic-wind` are available.

## Data

Lists are the default: `list`, `car`, `cdr`, `cons`, `append`, `reverse`, `length`,
`list-ref`, `first`/`second`/`third`, `last`, `take`, `drop`, `member`, `assoc`, `sort`,
`flatten`, `map`, `filter`, `foldl`, `reduce`, `for-each`, `range`, `transduce`.

Hashes are **immutable** — `hash-insert` returns a new hash and leaves the original
alone:

```scheme
(define h (hash "size" 2.0))
(hash-insert h "location" (list 0 0 1))   ; a new hash
```

`hash-ref` raises when the key is missing; `hash-try-get` returns `#false` instead.
Also `hash-contains?`, `hash-keys->list`, `hash-values->list`.

Vectors are immutable via `vector`; use `mutable-vector` with `vector-set!` when you
need in-place updates.

Strings: `string-append`, `string-length`, `substring`, `split-many` (**not**
`string-split`), `trim`, `starts-with?`, `string-contains?`, `string-upcase`,
`string->number`, `number->string`, `string->list`, `list->string`.

Numbers: `+ - * /`, `sqrt`, `expt`, `floor`, `round`, `abs`, `min`, `max`, `modulo`,
`quotient`, `even?`, `odd?`, `exact->inexact`. Steel keeps integers exact, so use
floats (`2.0`, `0.5`) wherever Blender expects one.

`#<void>` is what a Blender `None` becomes. Test it with `(void? x)` — that is how you
check whether a collection lookup missed.

## What the sandbox removes

Disabled, and raising if called: `load`, `eval`, `eval-string`, `eval-file`, `command`,
`spawn-process`, `open-input-file`, `open-output-file`, `display`, `displayln`, `write`,
`writeln`, `print`, `println`, `read`, `read-line`, `get-environment-variable`,
`set-environment-variable!`.

The `steel/process`, `steel/git`, `steel/meta`, `steel/fs`, `steel/ports`, `steel/io`,
`steel/tcp`, `steel/http`, `steel/polling`, and `steel/threads` modules are replaced
with empty ones.

There is no console: printing is disabled, so return values instead of displaying them.

## Budgets

Evaluation stops at `timeout_secs` (120 by default, 3600 maximum), enforced by a
watchdog that interrupts a runaway loop. Results are bounded at 16 levels of depth,
10,000 items, and 256 KiB of display text; exceed one and the value is rejected rather
than truncated, so return summaries rather than whole scene graphs.
