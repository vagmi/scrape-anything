export interface SessionListItem {
  session_id: string;
  url: string;
  host: string;
  intention: string;
  status: string;
  created_at: number;
  updated_at: number;
  item_count: number;
}

export interface Turn {
  turn_index: number;
  role: string; // extension | user | llm | tool
  kind: string; // signals | message | assistant | tool_response | result
  payload: any;
  created_at: number;
}

export interface SessionDetail {
  session_id: string;
  url: string;
  host: string;
  route: string;
  intention: string;
  status: string;
  created_at: number;
  updated_at: number;
  turns: Turn[];
}

export interface Artifact {
  artifact_id: string;
  kind: string;
  item_count: number;
  items: any[];
  updated_at: number;
}

export interface OriginFileMeta {
  path: string;
  size: number;
  updated_at: number;
}

export interface OriginFiles {
  origin: string;
  files: OriginFileMeta[];
}

export interface OriginFile {
  origin: string;
  path: string;
  content: string;
  updated_at: number;
}
