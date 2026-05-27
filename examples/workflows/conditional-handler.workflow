# Conditional branching workflow.
#
# 1. The `classify` step asks the AI to decide whether the
#    input is a question (returns "question") or a request
#    (returns "request"). Errors from the AI take the
#    failure edge instead.
# 2. On success the workflow routes to `handle_question`,
#    which answers it.
# 3. On failure the workflow routes to `report_error`, which
#    formats the failure for the operator.
#
# Edge conditions:
#   - `success`  — fires when the source step's dispatch
#                  returned OK.
#   - `failure`  — fires when the source step's dispatch
#                  returned ERR (transport, deadline, or a
#                  responder error envelope).
#   - `always`   — fires either way (used for cleanup /
#                  always-run join steps; not used here).
#   - `parallel` — fan-out fork (used in
#                  `parallel-research.workflow`).
#
# To run:
#   relix workflow run conditional-handler --input "What is the airspeed of an unladen swallow?"

name: conditional-handler
version: 1
description: Route to a handler based on whether classification succeeds or fails.

agents:
  classify:
    peer: ai
    capability: ai.chat
    input: "session-default|Classify this input — reply `question` or `request`: {{workflow.input}}|"
    output: classify

  handle_question:
    peer: ai
    capability: ai.chat
    input: "session-default|Answer this question: {{workflow.input}}|"
    output: answer

  report_error:
    peer: ai
    capability: ai.chat
    input: "session-default|Apologise to the operator. Classification step failed with: {{classify.output}}|"
    output: apology

flow:
  start: classify
  edges:
    - { from: classify, to: handle_question, condition: success }
    - { from: classify, to: report_error, condition: failure }
  result: "{{answer.output}}"
