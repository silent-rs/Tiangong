// sidecar 通信封装：bridge.call("sidecar.<op>")，业务失败以 {ok:false, summary} 返回。
import { createTiangongBridge, type HostBridge } from '@tiangong/plugin-sdk';

let bridgePromise: Promise<HostBridge> | null = null;

function getBridge(): Promise<HostBridge> {
  bridgePromise ??= createTiangongBridge();
  return bridgePromise;
}

export type EntryKind = 'dir' | 'file' | 'symlink';

export interface DirEntry {
  name: string;
  path: string;
  kind: EntryKind;
  size: number;
  modified_ms: number;
}

export interface DirListing {
  path: string;
  entries: DirEntry[];
  truncated: boolean;
}

export interface FileContent {
  path: string;
  kind: 'text' | 'image' | 'binary';
  size: number;
  modified_ms: number;
  hash?: string;
  content?: string;
  base64?: string;
  mime?: string;
  too_large: boolean;
}

export interface WriteResult {
  path: string;
  size: number;
  modified_ms: number;
  hash: string;
}

export class FilesApi {
  constructor(private readonly workspace: string) {}

  private async call<T>(operation: string, payload: Record<string, unknown>): Promise<T> {
    const bridge = await getBridge();
    const raw = await bridge.call(
      `sidecar.${operation}`,
      JSON.stringify({ ...payload, workspace: this.workspace }),
    );
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    if (parsed.ok === false) {
      throw new Error(String(parsed.summary ?? '操作失败'));
    }
    return parsed as T;
  }

  listDir(path: string) {
    return this.call<DirListing>('list_dir', { path });
  }

  readFile(path: string) {
    return this.call<FileContent>('read_file', { path });
  }

  writeFile(path: string, content: string, expectedHash?: string) {
    return this.call<WriteResult>('write_file', {
      path,
      content,
      expected_hash: expectedHash ?? null,
    });
  }

  create(path: string, kind: 'file' | 'dir') {
    return this.call<{ path: string }>('create', { path, kind });
  }

  rename(from: string, to: string) {
    return this.call<{ path: string }>('rename', { from, to });
  }

  remove(path: string) {
    return this.call<Record<string, never>>('delete', { path });
  }
}
