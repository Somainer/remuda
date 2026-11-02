# cb-requested-by real-condition (2026-11-02)

A scratch Hub on loopback and two scratch Nodes, both from this branch's
build; no `remuda dev`, no herdr. A REAL claude-sdk Main on the first Node ran
on a gateway-served model that answers (the id is recorded in the run's raw
proof, which is not committed). The owner drove the PWA from WebKit iPhone 13
through the shared Playwright run-server — never a locally launched browser.

This note records the round-3 run: `requestedBy` stores a reference only, and
the owner's words are read from the Hub's command row for Human reads.

1. **The owner's own words.** From the PWA the owner asked the seat to dispatch
   a worker onto the OTHER Node, which is cross-host and needs the D-017
   approval. The card stored `rootTurn {commandId, origin: human,
   continuation: none}` and `requestedBy {commandId}` with the same command
   id. The owner's read of `/v1/interactions` carried a 277-character
   `excerpt` of the owner's input, hydrated from that command row. In WebKit
   iPhone 13, `/approvals` (compact `/m/inbox`) rendered 「你说：…」 with no
   "agent 自发" tag, and the card's approval buttons were live.
2. **The same card, read by a Bot.** A device logged in as a Bot read the
   card list un-hydrated: `requestedBy {commandId}` alone, and the raw body
   held none of the owner's words.
3. **A non-owner turn.** After the owner's turn settled, the seat's own
   credential delivered an input (agent origin), so the seat acted in a turn it
   started itself. Its dispatch opened a second card with
   `rootTurn {origin: agent}` and `requestedBy {origin: agent}`; the PWA
   rendered the 「agent 自发」 tag and no owner words.
4. **Where the words are stored.** A scan of every table and column of the
   run's Hub database found the owner's words in exactly two places: the
   owner's command row, and the seat's own user message in its journal (the
   open and its replace). No task doc, roster row, card or other journal
   payload held them.

Both cards were denied after the screenshots, so nothing was provisioned.
Raw artifacts, not committed: `scratch/proof/reqby-r3-rc.txt`,
`scratch/proof/reqby-r3-approvals-owner-webkit-iphone13.png`,
`scratch/proof/reqby-r3-approvals-agent-webkit-iphone13.png`,
`scratch/proof/reqby-r3-rc-evidence.json`.
