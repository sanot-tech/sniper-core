# =============================================================================
# Bug report template
# =============================================================================
#
# The first question is the one that matters, and it is first on purpose:
#
# "Was the bot wrong, or did the measurement of the bot change?"
#
# Every bug in a trading system falls into one of those two, and they have
# completely different responses. The first is a defect and needs a fix. The
# second is usually a *correct* change whose consequences were not understood,
# and the fix belongs in the assumptions rather than in the code. Reporting them
# identically wastes the one thing a maintainer has, which is context.
# =============================================================================

name: Bug report
description: Something behaves differently from what it should
title: "[bug] "
labels: ["bug"]
body:
  - type: markdown
    attributes:
      value: |
        If the bot did nothing at all for a while, that is usually not a bug —
        a quiet market and a broken bot look identical from outside. Say so here
        and it will be investigated as a silence rather than as a crash.

  - type: dropdown
    id: kind
    attributes:
      label: What kind of wrong is this?
      description: The first option is for a defect; the second is for a correct
        change whose consequences were not understood. They need different fixes.
      options:
        - The bot did something wrong (bad trade, wrong size, bad decode)
        - The bot is fine but a measurement of it changed (quote, PnL, progress)
        - The bot stopped working entirely
        - It is not clear yet
    validations:
      required: true

  - type: textarea
    id: what
    attributes:
      label: What happened
      description: What you observed, and what you expected instead.
      placeholder: |
        The bot bought 0.5 SOL when the config says 0.02.
        Expected: 0.02 SOL, refused above the live cap.
    validations:
      required: true

  - type: textarea
    id: repro
    attributes:
      label: How to reproduce it
      description: |
        A command, a log line, a config file, or a transaction signature.
        A signature is the most useful thing you can provide — it is the only
        form that lets anyone see exactly what the bot saw.
      placeholder: |
        Signature: 5wHu1rJqJ9rD8nQvB3xK2mL7pR4tY6uI9oP0aS1dF2g
        Mode: live
        Position: 0.02 SOL
    validations:
      required: true

  - type: dropdown
    id: mode
    attributes:
      label: Which mode was the bot in?
      options:
        - dry-run
        - paper
        - devnet
        - live
    validations:
      required: true

  - type: textarea
    id: logs
    attributes:
      label: Relevant output
      description: |
        Logs from around the event. Trim what you can — a paste that needs
        scrolling is a paste that does not get read.
      render: shell
    validations:
      required: false

  - type: input
    id: commit
    attributes:
      label: Commit or version
      description: The exact `git rev-parse HEAD`, or the release tag.
      placeholder: 9d27b88
    validations:
      required: false

  - type: textarea
    id: notes
    attributes:
      label: Anything else
      description: |
        What you were doing, what changed just before, what you expected
        instead of what happened. The last one is usually the useful part.
    validations:
      required: false

  - type: checkboxes
    id: secrets
    attributes:
      label: Before submitting
      options:
        - label: |
            I have removed API keys, private keys and signatures from anything
            pasted above. A leaked Solana private key means losing every dollar
            in that wallet, and a leaked API key means someone else spends your
            quota.
          required: true