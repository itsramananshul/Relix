# Parallel fan-out + converging join workflow.
#
# 1. The `frame` step turns the operator's input into a
#    research framing prompt.
# 2. Three sibling branches fan out concurrently (one
#    capability call each, all in flight at the same time):
#      - `historical`  — asks for the historical context.
#      - `technical`   — asks for the technical details.
#      - `controversy` — asks for the contested aspects.
# 3. After ALL three siblings finish, the `synthesise` join
#    step combines their outputs into a single briefing.
#
# Why both `parallel:` and `success:` edges are present:
#   `parallel:` fires the three branches concurrently from
#   `frame`. The three sibling → `synthesise` edges use
#   `success:` so the join only runs once each sibling
#   succeeds. The executor's visited-set guarantees
#   `synthesise` runs exactly once even though three success
#   edges target it.
#
# To run:
#   relix workflow run parallel-research --input "the history of the Suez canal"

name: parallel-research
version: 1
description: Fan three research angles out in parallel, then synthesise.

agents:
  frame:
    peer: ai
    capability: ai.chat
    input: "session-default|Briefly frame the topic: {{workflow.input}}|"
    output: frame

  historical:
    peer: ai
    capability: ai.chat
    input: "session-default|Historical context for: {{frame.output}}|"
    output: historical

  technical:
    peer: ai
    capability: ai.chat
    input: "session-default|Technical detail on: {{frame.output}}|"
    output: technical

  controversy:
    peer: ai
    capability: ai.chat
    input: "session-default|Contested aspects of: {{frame.output}}|"
    output: controversy

  synthesise:
    peer: ai
    capability: ai.chat
    input: "session-default|Combine these into a one-paragraph briefing.\nHISTORICAL: {{historical.output}}\nTECHNICAL: {{technical.output}}\nCONTROVERSY: {{controversy.output}}|"
    output: briefing

flow:
  start: frame
  edges:
    - { from: frame, to: historical, condition: parallel }
    - { from: frame, to: technical, condition: parallel }
    - { from: frame, to: controversy, condition: parallel }
    - { from: historical, to: synthesise, condition: success }
    - { from: technical, to: synthesise, condition: success }
    - { from: controversy, to: synthesise, condition: success }
  result: "{{briefing.output}}"
