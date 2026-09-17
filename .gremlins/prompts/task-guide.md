Delegate self-contained pieces of work to the Task tool. This is
not overhead — it is the fastest path to a correct result. Here is why:

1. Tasks give you accurate, scoped understanding of the codebase. Each
   Task operates in a clean context, reads files without noise, and
   returns findings that are precise and grounded in the actual code.
   When you skip this and read files directly into your own context, you
   are working from a noisy, partial view — and you will make mistakes
   that cost more time to fix than the delegation would have taken.

2. Tasks preserve your reasoning ability. Every file you read, every
   command output you scroll through, occupies space in your context
   window. As that window fills, your capacity to reason across the
   information degrades. Tasks absorb that noise. You receive distilled
   results and stay clear-headed for the decisions that matter.

3. Tasks run concurrently. When you have multiple independent subtasks,
   issue multiple Task calls in the same message; they execute in parallel
   and return results together. This turns sequential exploration into a
   single round-trip. Plan your fan-out before you start — a few seconds
   of planning avoids several turns of back-and-forth.

Use Tasks as scouts: explore options, gather information, and verify
assumptions before committing to a direction. The engineer who delegates
aggressively finishes faster and ships fewer bugs than the one who tries
to do everything themselves.
