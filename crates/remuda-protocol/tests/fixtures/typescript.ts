// Source: protocol.md sections 1.3, 2.5, 4.1, 7.2, 7.4; compile against Rust-generated types.
import type {
  BinaryHeader, CarrierSpec, CommandState, InstanceOpenTerminalParams,
  Observation, RpcRequest, TtyAttachParams,
} from '../../../../web/src/types/generated';

const carrier: CarrierSpec = { type: 'claude-bg', inputDelivery: 'deferred-argv', argvInputPolicy: 'explicit-non-secret' };
const open: InstanceOpenTerminalParams = {
  instanceId: 'ins_fixture', backgroundJobId: 'job-fixture', allowWake: true,
  carrier: {
    backend: 'herdr', session: 'remuda-test',
    server: { binaryPath: '/opt/herdr', version: 'fixture', digest: 'sha256:fixture', protocolVersion: 'fixture', serverIdentity: 'obj_fixture', serverEpoch: 'epoch_fixture', representation: 'rendered-ansi' },
  },
};
const header: BinaryHeader = { channel: 'tty-output', streamUuid: 'fixture', offset: '9007199254740993', payloadLength: 3 };
const tty: TtyAttachParams = { instanceId: 'ins_fixture', processGeneration: '1', mode: 'read', previousStreamId: null, afterOffset: null };
// @ts-expect-error Opening an attach pane requires explicit true.
const noWake: InstanceOpenTerminalParams = { ...open, allowWake: false };
// @ts-expect-error tty.attach cannot carry wake/launch options.
const implicitWake: TtyAttachParams = { ...tty, allowWake: true };
// @ts-expect-error Lossless wire counters are strings.
const badOffset: BinaryHeader = { ...header, offset: 9007199254740993 };
// @ts-expect-error Unknown delivery uses Command.resolution, not a fourth Command.state.
const badState: CommandState = 'unknown';
// @ts-expect-error Background carrier cannot select stdio for first input.
const badCarrier: CarrierSpec = { type: 'claude-bg', inputDelivery: 'stdio', argvInputPolicy: 'explicit-non-secret' };
function inspect(request: RpcRequest, observation: Observation): void {
  if (request.method === 'instance.open_terminal') {
    const wake: true = request.params.payload.allowWake;
    void wake;
  }
  if (observation.kind === 'message') {
    const role: string = observation.payload.role;
    void role;
  }
}
void [carrier, open, header, tty, noWake, implicitWake, badOffset, badState, badCarrier, inspect];
