# Sequential two-agent workflow.
#
# 1. The `responder` step asks the AI peer to answer the
#    operator's input.
# 2. The `summarizer` step asks the AI to compress that
#    answer down to one sentence.
#
# The summarizer's input interpolates `{{responder.output}}`,
# which is the response body the `responder` step bound to
# its `output: responder` slot. `flow.result` then projects
# the summarizer's output as the workflow's final return.
#
# To run:
#   relix workflow run chat-then-summarize --input "Why is the sky blue?"
#
# Both steps target the canonical `ai` peer alias. Override
# either step's `peer:` field to point at a different AI peer
# (e.g. a domain-specialised one) without rewriting the flow
# graph.

name: chat-then-summarize
version: 1
description: Answer the user's question, then compress the answer to one line.

agents:
  responder:
    peer: ai
    capability: ai.chat
    input: "session-default|{{workflow.input}}|"
    output: responder

  summarizer:
    peer: ai
    capability: ai.chat
    input: "session-default|Summarise this in one sentence: {{responder.output}}|"
    output: summary

flow:
  start: responder
  edges:
    - { from: responder, to: summarizer, condition: success }
  result: "{{summary.output}}"
