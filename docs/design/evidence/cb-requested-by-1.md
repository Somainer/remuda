# cb-requested-by real-condition (2026-11-02)

A scratch Hub on loopback and two scratch Nodes, both from this branch's
build; no `remuda dev`, no herdr. A REAL claude-sdk Main on the first Node ran
on a gateway-served model that answers (the id is recorded in the run's raw
proof, which is not committed). The owner drove the PWA from WebKit iPhone 13
through the shared Playwright run-server — never a locally launched browser.

1. **The owner's own words.** From the PWA the owner asked the seat to dispatch
   a worker onto the OTHER Node, which is cross-host and needs the D-017
   approval. The card recorded `rootTurn {commandId, origin: human,
   continuation: none}` and `requestedBy` with the same command id plus a
   277-character excerpt of the owner's input. In WebKit iPhone 13,
   `/approvals` (compact `/m/inbox`) rendered 「你说：…」 with no "agent 自发"
   tag, and the card's approval buttons were live.
2. **A non-owner turn.** After the owner's turn settled, the seat's own
   credential delivered an input (agent origin), so the seat acted in a turn it
   started itself. Its dispatch opened a second card with
   `rootTurn {origin: agent}` and `requestedBy {origin: agent}`; the PWA
   rendered the 「agent 自发」 tag and no owner words.

Both cards were denied after the screenshots, so nothing was provisioned.
Raw artifacts, not committed: `scratch/proof/reqby-rc.txt`,
`scratch/proof/reqby-approvals-owner-webkit-iphone13.png`,
`scratch/proof/reqby-approvals-agent-webkit-iphone13.png`,
`scratch/proof/reqby-rc-evidence.json`.
