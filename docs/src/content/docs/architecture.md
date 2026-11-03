---
title: How routing works
description: How Noria chooses accounts, keeps prompt caches warm, and switches when limits are reached.
order: 5
category: How it works
---

Noria is a **smart proxy running on your computer**. Your normal Codex and Claude Code clients talk to Noria instead of directly to their provider. Noria picks one of your already-authorized accounts and forwards the original request.

The goal is simple: **make better use of the limits across your accounts while keeping account switching out of your workflow.**

```text
  Your Codex / Claude Code session
                 │
                 ▼
         Noria · localhost
        /                  \
   Codex pool          Claude pool
    A  B  C              D  E
        │                  │
        ▼                  ▼
     OpenAI             Anthropic
```

Codex requests go only to Codex accounts, and Claude Code requests go only to Claude accounts. Noria never silently changes your model or provider.

## 1. A new session needs an account

When you start a new conversation, Noria considers every eligible account of the correct provider.

It looks at four things:

1. **Remaining allowance.** Is the account allowed to make the request? Both the five-hour and weekly windows (plus any applicable model limits) must have capacity.
2. **Reset times.** How soon will unused allowance expire? Five-hour and weekly clocks are independent, even on the same account.
3. **Current work.** Is this account already handling requests or supporting active conversations? Avoid piling new sessions onto an account whose capacity is likely to be consumed by existing work.
4. **Observed consumption.** Noria learns roughly how much each completed request consumes from each window and uses that information to estimate useful capacity.

Noria scores the eligible accounts using a bounded lookahead. The score favors allowance that is both **usable now** and **at risk of expiring soon**, without overloading one account.

### Example: 5% can beat 80%

Consider two Codex accounts:

| | Account A | Account B |
| --- | --- | --- |
| Weekly allowance left | **5%** | 80% |
| Weekly reset | **In 45 minutes** | In 6 days |
| Five-hour window | Has capacity | Has capacity |

For a suitable **new session**, A can be a smarter choice. Its last 5% is about to reset, while B's 80% will still be around for days. Saving A's quota would waste an opportunity to use it.

That doesn't mean Noria always picks A. If existing sessions are likely to use up its last 5%, or the other quota window would prevent useful work, B may be better. The decision considers both windows, outstanding requests, and expected demand.

**Important:** Noria doesn't compare five-hour percentages to weekly percentages as if they were equivalent tokens. It estimates capacity within each window separately.

## 2. Once assigned, a conversation stays put

After the first successful request, Noria binds that conversation to its account.

```text
  Conversation 42
     Request 1 ───→ Account A
     Request 2 ───→ Account A
     Request 3 ───→ Account A
     Request 4 ───→ Account A
```

This is **session affinity** (or *sticky routing*). It matters because prompt caches are account-level. Switching on every request would throw away cache reuse and may make work slower and less efficient.

Noria doesn't rebalance an active conversation just because another account becomes more attractive or resets. The original binding is persisted and survives a Noria restart.

## 3. A limit triggers a handoff

Suppose Account A hits its weekly limit on the next request:

```text
  Conversation 42 ──→ Account A ──→ quota rejected
                                        │
                           find eligible Account B
                                        │
  Pending request ──────────────────────→ B
  Following requests ───────────────────→ B
```

Noria marks the applicable limit as blocked, chooses another usable account **of the same provider**, and retries the *unfulfilled request* when it's safe to do so. The conversation then sticks to B. If A resets later, Noria doesn't bounce that conversation back.

The cache on B starts cold and warms through normal requests. Noria doesn't attempt to transfer caches between accounts.

**Where automatic continuation stops:** If the client has already received part of a model response, Noria cannot transparently splice another account's output into it or replay completed tool actions. In that case, the client may need a normal Continue request. This is the same practical boundary you encounter when manually changing accounts mid-conversation.

## 4. Each limit has its own clock

Every account can have different reset times for its five-hour and weekly windows. A first-use window starts only when real work begins—Noria never sends dummy prompts to force a reset clock to start.

An account is available only when **all applicable limits** permit the request. A five-hour reset does not clear an exhausted weekly limit, and a healthy weekly allowance cannot bypass a model-specific limit.

Telemetry isn't always perfect. Noria uses recent provider usage readings, but an actual quota rejection from the provider takes priority. Missing readings are treated as **unknown**, not as an unlimited allowance. If no account can serve a request, Noria reports the shortage and a known next reset if one is available.

## 5. Why routing stays local

The background Rust service owns the account pool, session bindings, quota observations, and scheduling decisions. Each authorized account has a separate native sign-in profile. The Swift macOS menu bar and Rust Windows/Linux tray display CLI state and provide controls.

The proxy runs on loopback (`localhost`). The model request still goes to OpenAI or Anthropic using the chosen account. Noria is **not a cloud relay, a model host, or a way to bypass a provider's actual usage caps**.

## The tradeoff

Noria optimizes how *new* sessions are distributed, but protecting an active conversation's cache comes first. One long healthy conversation might leave some capacity unused in other accounts. Multiple independent conversations give Noria more opportunities to distribute work intelligently.

That's by design: **a sticky, quota-aware router—not a round-robin load balancer.**

For the constraints around provider compatibility, credentials, and partially streamed requests, see [Security & limitations](/docs/security-and-limitations).
