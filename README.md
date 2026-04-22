# 🛠️ Specification: `cargo-impact`
**Subtitle:** *Predictive Regression Analysis & Verification Mapping for Rust*

## 1. Core Philosophy
`cargo-impact` moves the developer from "Running all tests and hoping for the best" to **Surgical Verification**. It treats a code change as a "stone thrown into a pond" and calculates exactly which ripples hit which shores (tests, docs, APIs).

It answers the critical question: *"I changed X; what is the minimum set of things I must check to be 99% sure I didn't break Y?"*

---

## 2. Technical Feature Set

### A. The "Blast Radius" Engine (Static Analysis)
Instead of just looking at files, `cargo-impact` analyzes **symbols**.
*   **Symbol Tracking:** It uses `syn` and `cargo metadata` to identify exactly which functions, structs, or traits were modified in the `git diff`.
*   **Call-Graph Traversal:** It performs a reverse-lookup. If `fn calculate_tax()` was changed, it finds every call site of that function across the workspace.
*   **Trait Ripple Effect:** If a trait definition was changed, it flags every implementation of that trait as "potentially unstable."

### B. Affected Test Selection (Surgical Testing)
Running `cargo test` on a large project is a vibe-killer.
*   **Direct Mapping:** Matches changed files to corresponding `tests/*.rs` or `#[cfg(test)]` blocks.
*   **Indirect Mapping:** If `src/auth.rs` was changed, and `tests/api_integration.rs` calls a function in `auth.rs`, it marks that test as "High Priority."
*   **Action:** It generates a filtered test command: `cargo test test_auth_login test_api_handshake`.

### C. Runtime Surface Mapping (The "Exposed" Layer)
It identifies which "Public Faces" of the application are affected.
*   **API Endpoints:** If using `axum` or `actix`, it maps changed logic to the routes that trigger it.
*   **CLI Commands:** If using `clap`, it identifies which subcommands execute the modified code paths.
*   **Public API:** Flags if a `pub` function signature changed, signaling that downstream crates/users are impacted.

### D. Documentation Drift Detection
AI often updates code but forgets the docs.
*   **Keyword Association:** It scans `.md` files in `/docs` or doc-comments (`///`) for keywords associated with the changed symbols.
*   **Notification:** *"You changed the `PaymentGateway` logic; the `docs/billing.md` file likely contains outdated information."*

---

## 3. CLI Interface (UX)

```bash
# Analyze the current git diff and show the blast radius
cargo impact

# Only run the tests that are likely affected by the current changes
cargo impact --test

# Generate a "Verification Checklist" for the AI to follow
cargo impact --checklist

# Analyze a specific commit range
cargo impact --since a1b2c3d
```

### The "Blast Radius" Report (Output):
When running `cargo impact`, the tool outputs a categorized risk assessment:

**🔴 HIGH RISK (Directly Modified)**
- `src/core/engine.rs` $\rightarrow$ `fn process_event()`
- `src/models/user.rs` $\rightarrow$ `struct UserProfile`

**🟡 MEDIUM RISK (Indirectly Affected)**
- **Tests:** `tests/integration_tests.rs` (calls `process_event`)
- **API:** `GET /api/v1/user/profile` (depends on `UserProfile`)
- **Crates:** `crate-api-gateway` (depends on `core` crate)

**🔵 LOW RISK (Documentation/Peripheral)**
- `docs/architecture.md` (mentions `process_event`)
- `src/cli.rs` $\rightarrow$ `cmd sync`

---

## 4. Vibe Coding Workflow Integration

This completes the **Context $\rightarrow$ Code $\rightarrow$ Verify** loop:

1.  **Context:** `cargo context --fix | pbcopy` $\rightarrow$ AI generates a fix.
2.  **Apply:** Developer applies the AI's code.
3.  **Impact:** Developer runs `cargo impact`.
4.  **Verify:** 
    *   Developer sees that `cargo impact` flagged a specific integration test and one API endpoint.
    *   Developer runs `cargo impact --test` (takes 5 seconds instead of 5 minutes).
    *   Developer tells the AI: *"The fix works, but `cargo-impact` says this might affect the `/user/profile` endpoint. Can you double-check the logic for that specific surface?"*

## 5. Summary Table: Context vs. Impact

| Feature | `cargo-context` (The Input) | `cargo-impact` (The Output) |
| :--- | :--- | :--- |
| **Goal** | Maximize AI understanding | Minimize human verification effort |
| **Focus** | What is the AI looking at? | What did the AI touch? |
| **Primary Tool** | `git diff` + `cargo metadata` | Call-graph + Symbol analysis |
| **Key Output** | A Markdown Context Pack | A Blast Radius Risk Report |
| **Vibe Shift** | No more copy-pasting files | No more "test all and pray" |
