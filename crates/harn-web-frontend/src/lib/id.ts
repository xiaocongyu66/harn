// 连接级单调 id 生成器。重连即重建整个会话（后端不重播已消费事件），
// 故计数器只在单次连接内保证唯一；重连后重置是既有设计。

export class IdGenerator {
  private seq = 0;
  private toolSeq = new Map<string, number>();

  user(): string {
    return `u-${this.seq++}`;
  }

  assistant(turn: number, agentId: string): string {
    return `a-${turn}:${agentId}`;
  }

  /** (turn,agent,name) 内单调，用作 toolCallId 与 FIFO 配对键。 */
  toolCall(turn: number, agentId: string, name: string): string {
    const key = `${turn}:${agentId}:${name}`;
    const n = this.toolSeq.get(key) ?? 0;
    this.toolSeq.set(key, n + 1);
    return `t-${key}:${n}`;
  }

  approval(toolCallId: string): string {
    return `appr-${toolCallId}`;
  }
}
