export const meta = {
  name: 'spike-wf-runfail',
  description: 'r-ux-w run-level failure spike',
  phases: [{ title: 'Boom' }],
}

phase('Boom')
const r = await agent('Run this exact shell command and nothing else: echo ok-boom-one', { label: 'boom:one', phase: 'Boom' })
throw new Error('spike-run-failure-after-agent')