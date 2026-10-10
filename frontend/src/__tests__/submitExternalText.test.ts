import { beforeEach, describe, expect, it, vi } from 'vitest';

import { useStore } from '@/store/useStore';
import { emptyInputCache } from '@/store/inputCache';

/**
 * 插件 sendText 的「用户普通 Enter」语义：session.input.sendText 经
 * submitExternalText 投递——保护现有草稿（先入队不覆盖）、执行中入队
 * （不立即引导，由用户决定）、空闲立即发送、信任模式用界面当前选择。
 */
const KEY = 'session-sendtext-test';

function prepareSession(options: { running?: boolean; draft?: string } = {}) {
  useStore.setState({
    activeSessionId: KEY,
    inputCaches: { [KEY]: { ...emptyInputCache(), text: options.draft ?? '' } },
    inputQueues: {},
    sessionRunStatuses: options.running ? { [KEY]: 'executing' } : {},
  });
}

function spyActions() {
  // 只替换 sendMessage（避免真实 Tauri 调用）；入队走真实实现，
  // 行为经队列结果断言。
  const send = vi.fn().mockResolvedValue(true);
  useStore.setState({ sendMessage: send as never });
  return { send };
}

describe('submitExternalText（插件 sendText 的普通 Enter 语义）', () => {
  beforeEach(() => {
    prepareSession();
  });

  it('Agent 空闲：立即发送，透传界面当前信任模式', () => {
    const { send } = spyActions();
    useStore.getState().submitExternalText(KEY, '开始创建插件', 'supervised');
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0][1]).toBe('开始创建插件');
    expect(send.mock.calls[0][4]).toBe('supervised');
    expect(useStore.getState().inputQueues[KEY] ?? []).toHaveLength(0);
  });

  it('Agent 运行中：进入前端队列，不发送不引导', () => {
    const { send } = spyActions();
    prepareSession({ running: true });
    useStore.getState().submitExternalText(KEY, '再来一个插件', 'full_trust');
    expect(send).not.toHaveBeenCalled();
    const queue = useStore.getState().inputQueues[KEY] ?? [];
    expect(queue).toHaveLength(1);
    expect(queue[0].text).toBe('再来一个插件');
    // 插件消息入队后草稿清空（等待自动放行或用户立即引导）。
    expect(useStore.getState().inputCaches[KEY].text).toBe('');
  });

  it('输入框已有草稿：先入队保护，不覆盖', () => {
    const { send } = spyActions();
    prepareSession({ running: true, draft: '用户未发送的草稿' });
    useStore.getState().submitExternalText(KEY, '插件消息', 'full_trust');
    expect(send).not.toHaveBeenCalled();
    const queue = useStore.getState().inputQueues[KEY] ?? [];
    expect(queue).toHaveLength(2);
    expect(queue[0].text).toBe('用户未发送的草稿');
    expect(queue[1].text).toBe('插件消息');
  });

  it('连续两次调用：两条都进队列，不丢失不覆盖', () => {
    spyActions();
    prepareSession({ running: true });
    useStore.getState().submitExternalText(KEY, '第一条', 'full_trust');
    useStore.getState().submitExternalText(KEY, '第二条', 'full_trust');
    const queue = useStore.getState().inputQueues[KEY] ?? [];
    expect(queue.map((message) => message.text)).toEqual(['第一条', '第二条']);
  });

  it('发送事务中第二条到达：第一条不重复，第二条直接入队', async () => {
    // 模拟真实 sendMessage：进入即置 is_sending 并保持挂起。
    let release!: () => void;
    const gate = new Promise<void>((resolvePromise) => {
      release = resolvePromise;
    });
    const send = vi.fn().mockImplementation(async () => {
      useStore.setState((current) => ({
        inputCaches: {
          ...current.inputCaches,
          [KEY]: { ...current.inputCaches[KEY], is_sending: true },
        },
      }));
      await gate;
      return true;
    });
    useStore.setState({ sendMessage: send as never });

    // 第一条：空闲路径，写入草稿并开始发送（挂起中）。
    useStore.getState().submitExternalText(KEY, '第一条', 'full_trust');
    expect(send).toHaveBeenCalledTimes(1);
    // 第二条：发送事务中到达——不得把第一条草稿再次入队。
    useStore.getState().submitExternalText(KEY, '第二条', 'full_trust');
    const queue = useStore.getState().inputQueues[KEY] ?? [];
    expect(queue.map((message) => message.text)).toEqual(['第二条']);

    // 第一条发送完成：队列只剩第二条，等待自动放行。
    release();
    await Promise.resolve();
    expect(useStore.getState().inputQueues[KEY] ?? []).toHaveLength(1);
    expect(useStore.getState().inputQueues[KEY]?.[0].text).toBe('第二条');
  });

  it('Agent 空闲但已有草稿：草稿先入队保留，外部文本立即发送', () => {
    const { send } = spyActions();
    prepareSession({ draft: '用户未发送的草稿' });
    useStore.getState().submitExternalText(KEY, '插件消息', 'full_trust');
    // 外部文本立即发送（不是入队等待）。
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0][1]).toBe('插件消息');
    // 用户草稿保留在队列中，未被覆盖丢失。
    const queue = useStore.getState().inputQueues[KEY] ?? [];
    expect(queue.map((message) => message.text)).toEqual(['用户未发送的草稿']);
  });

  it('空文本：不做任何投递', () => {
    const { send } = spyActions();
    useStore.getState().submitExternalText(KEY, '   ', 'full_trust');
    expect(send).not.toHaveBeenCalled();
    expect(useStore.getState().inputQueues[KEY] ?? []).toHaveLength(0);
    expect(useStore.getState().inputQueues[KEY] ?? []).toHaveLength(0);
  });

  it('携带附件：附件写入草稿并随发送投递（sendMessage 以草稿附件为准）', () => {
    const { send } = spyActions();
    const audio = {
      kind: 'audio' as const,
      source: '/Users/me/.tiangong/media/stt_rec_x.wav',
      original_name: '语音消息.wav',
      mime_type: 'audio/wav',
    };
    useStore.getState().submitExternalText(KEY, '你好', 'full_trust', [audio]);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0][2]).toEqual([audio]);
    // 草稿同步写入附件：真实 sendMessage 以同步后的草稿附件投递。
    const cache = useStore.getState().inputCaches[KEY];
    expect(cache.attachments).toEqual([audio]);
    expect(send.mock.calls[0][3]).toBe(cache.revision);
  });

  it('携带渲染声明：空闲发送透传 render，运行中入队保留并在放行时投递', async () => {
    const { send } = spyActions();
    const render = { plugin: 'volcengine', view: 'voice-bubble', data: { duration_s: 3 } };
    useStore.getState().submitExternalText(KEY, '你好', 'full_trust', [], render);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0][6]).toEqual(render);

    send.mockClear();
    prepareSession({ running: true });
    useStore.getState().submitExternalText(KEY, '排队的语音', 'full_trust', [], render);
    expect(send).not.toHaveBeenCalled();
    const queued = useStore.getState().inputQueues[KEY] ?? [];
    expect(queued[0].render).toEqual(render);

    // 空闲后放行：队列项按原样投递，渲染声明随之送达。
    useStore.setState({ sessionRunStatuses: {} });
    await useStore.getState().steerQueuedInputMessage(KEY, queued[0].id);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send.mock.calls[0][1]).toBe('排队的语音');
    expect(send.mock.calls[0][6]).toEqual(render);
  });
});
