---
description: Shared vocabulary and principles for designing deep modules - module, interface, depth, seam, adapter, leverage, locality - plus the deletion test, dependency categories for safe deepening, and the design-it-twice pattern for exploring alternative interfaces. Load when designing or improving a module's interface, deciding where a seam goes, making code more testable, or when another skill or agent needs the deep-module vocabulary.
---
Design **deep modules**: a lot of behaviour behind a small interface, placed at a clean seam, testable through that interface. Use this language and these principles wherever code is being designed or restructured. The aim is leverage for callers, locality for maintainers, and testability for everyone.

## Glossary (use these terms exactly)

Consistent language is the point — don't substitute "component", "service", "API", or "boundary".

- **Module**: anything with an interface and an implementation. Deliberately scale-agnostic: a function, class, package, or tier-spanning slice.
- **Interface**: everything a caller must know to use the module correctly — the type signature, but also invariants, ordering constraints, error modes, required configuration, and performance characteristics. ("API"/"signature" are too narrow: they name only the type-level surface.)
- **Implementation**: what's inside a module.
- **Depth**: leverage at the interface — how much behaviour a caller (or test) can exercise per unit of interface they must learn. **Deep** = lots of behaviour behind a small interface. **Shallow** = an interface nearly as complex as the implementation.
- **Seam** *(Feathers)*: a place where you can alter behaviour without editing in that place; the *location* where a module's interface lives. Where the seam goes is its own design decision, distinct from what goes behind it. (Avoid "boundary" — overloaded with DDD's bounded context.)
- **Adapter**: a concrete thing that satisfies an interface at a seam. Names *role* (what slot it fills), not substance.
- **Leverage**: what callers get from depth — more capability per unit of interface learned. One implementation pays back across N call sites and M tests.
- **Locality**: what maintainers get from depth — change, bugs, knowledge, and verification concentrate in one place. Fix once, fixed everywhere.

## Principles

- **Depth is a property of the interface, not the implementation.** A deep module can be internally composed of small, mockable parts; they just aren't part of the interface. A module can have **internal seams** (private, used by its own tests) as well as the external seam at its interface — don't expose internal seams just because tests use them.
- **The deletion test.** Imagine deleting the module. If complexity vanishes, it was a pass-through. If complexity reappears across N callers, it was earning its keep. Apply this to anything you suspect is shallow.
- **The interface is the test surface.** Callers and tests cross the same seam. Wanting to test *past* the interface means the module is probably the wrong shape.
- **One adapter means a hypothetical seam. Two adapters means a real one.** Don't introduce a seam unless something actually varies across it (typically production + test). A single-adapter seam is just indirection.
- When designing an interface, ask: can I reduce the number of methods? simplify the parameters? hide more complexity inside?

## Designing for testability

1. **Accept dependencies, don't create them** — `processOrder(order, paymentGateway)` is testable; a function that constructs its own gateway is not.
2. **Return results, don't produce side effects** — `calculateDiscount(cart): Discount` beats `applyDiscount(cart): void`.
3. **Small surface area** — fewer methods = fewer tests needed; fewer params = simpler setup.

## Dependency categories (for safe deepening)

When deepening a cluster of shallow modules, classify its dependencies — the category determines how the deepened module is tested across its seam:

1. **In-process** (pure computation, in-memory state): always deepenable; merge and test through the new interface directly. No adapter needed.
2. **Local-substitutable** (deps with real local stand-ins: embedded/in-memory DB, in-memory filesystem): deepenable if the stand-in exists; the test suite runs the stand-in, the seam stays internal.
3. **Remote but owned** (your own services across a network): define a port (interface) at the seam; production gets an HTTP/gRPC/queue adapter, tests get an in-memory adapter. The logic sits in one deep module even though it deploys across a network.
4. **True external** (third-party services you don't control): injected port; tests provide a mock adapter.

**Testing strategy: replace, don't layer.** Once tests exist at the deepened module's interface, old unit tests on the merged shallow modules are waste — delete them. New tests assert observable outcomes through the interface and survive internal refactors; a test that must change when the implementation changes is testing past the interface.

## Design it twice

Your first interface idea is unlikely to be the best (Ousterhout). For a module worth the effort, produce **2-3 radically different interface designs** before committing — in parallel sub-agents when available, sequentially otherwise. Give each a different constraint:

- Minimise the interface: 1-3 entry points, maximum leverage per entry point.
- Maximise flexibility: many use cases, room for extension.
- Optimise for the most common caller: make the default case trivial.
- (When cross-seam dependencies dominate) design around ports & adapters.

Each design specifies: the interface (including invariants, ordering, error modes), a caller usage example, what the implementation hides, the dependency/adapter strategy, and where leverage is high vs thin. Compare on **depth**, **locality**, and **seam placement**, then give ONE opinionated recommendation (or a justified hybrid) — the reader wants a strong read, not a menu.

## Anti-patterns

- Measuring depth as implementation-lines over interface-lines — rewards padding. Depth is leverage, not a ratio.
- Extracting pure functions "for testability" while the real bugs live in how they're called — that trades away locality and deepens nothing.
- Introducing ports/interfaces speculatively ("we might swap the DB") — one adapter is a hypothetical seam.
- Renaming without restructuring: calling a pass-through layer an "adapter" doesn't make the module deep. Apply the deletion test.
- Vocabulary drift mid-discussion ("component", "service", "boundary") — the shared terms exist so design conversations compose.
