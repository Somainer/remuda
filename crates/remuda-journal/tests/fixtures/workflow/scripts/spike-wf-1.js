export const meta = {
  name: 'spike-wf-1',
  description: 'r-ux-w two-phase signals spike',
  phases: [{ title: 'Alpha' }, { title: 'Beta' }],
}

phase('Alpha')
const a = await parallel([
  () => agent('Run this exact shell command and nothing else: echo hello-alpha-1; sleep 2; echo done-alpha-1', { label: 'alpha:one', phase: 'Alpha' }),
  () => agent('Run this exact shell command and nothing else: echo hello-alpha-2; sleep 3; echo done-alpha-2', { label: 'alpha:two', phase: 'Alpha' }),
])
phase('Beta')
const b = await parallel([
  () => agent('Reply with exactly the single word PONG and nothing else.', { label: 'beta:one', phase: 'Beta' }),
  () => agent('Run this exact shell command and nothing else: echo hello-beta-two', { label: 'beta:two', phase: 'Beta' }),
])
return { a, b }