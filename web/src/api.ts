// API client for lecture-distill JSON endpoints.

// ---------------------------------------------------------------------------
// Source types
// ---------------------------------------------------------------------------

export type SourceKind = 'transcript_day' | 'transcript_course' | 'note';
export type SourceStatus = 'ready' | 'processing' | 'failed';

export interface SourceRecord {
  id: string;
  kind: SourceKind;
  title: string;
  status: SourceStatus;
  created_at: string;
  updated_at: string;
  length?: string;
  path: string;
  metadata: Record<string, unknown>;
  last_error?: string;
  job_id?: string;
}

export interface SourcesResponse {
  sources: SourceRecord[];
}

export interface SourceActionResponse {
  status: string;
  source?: SourceRecord;
  source_id?: string;
  job_id?: string;
  error?: string;
  message?: string;
}

export interface AskResponse {
  status: string;
  answer: string;
  llm_used: boolean;
  source_id: string;
  fallback_reason?: string;
  retrieval?: unknown[];
}

export interface CanvasCourse {
  id: number;
  name: string;
  course_code: string;
  start_at?: string;
  end_at?: string;
  workflow_state: string;
  enrollment_term_id?: number;
  term_name: string;
  teachers: string[];
}

export interface CanvasCoursesResponse {
  status: string;
  courses: CanvasCourse[];
  count: number;
  error?: string;
}

export interface CourseDate {
  date: string;
  video_count: number;
  first_title?: string;
  last_title?: string;
}

export interface CourseDatesResponse {
  status: string;
  course_id: string;
  total_videos: number;
  dates: CourseDate[];
  error?: string;
}

// ---------------------------------------------------------------------------
// Process types
// ---------------------------------------------------------------------------

export type ProcessOutputKind = 'note_patch' | 'cheating_sheet';
export type ProcessStatus = 'ready' | 'processing' | 'failed';

export interface ProcessOutput {
  id: string;
  kind: ProcessOutputKind;
  status: ProcessStatus;
  title: string;
  path: string;
  diff_path?: string;
  base_source_id?: string;
  created_at: string;
  updated_at: string;
  last_error?: string;
  metadata?: Record<string, unknown>;
}

export interface ProcessRecord {
  id: string;
  title: string;
  status: ProcessStatus;
  created_at: string;
  updated_at: string;
  source_ids: string[];
  outputs: ProcessOutput[];
  last_error?: string;
  job_id?: string;
}

export interface ProcessesResponse {
  processes: ProcessRecord[];
}

export interface ProcessOutputContent {
  process_id: string;
  output: ProcessOutput;
  markdown: string;
  diff: string;
  retrieval?: RetrievalTrace[];
  has_base_note: boolean;
  artifact_path?: string;
}

export interface TimestampRange {
  video_id: string;
  label: string;
  start: number;
  end: number;
  text_preview: string;
}

export interface CourseDateIndex {
  date: string;
  title: string;
  summary: string;
  keywords: string[];
  concepts: string[];
  timestamp_ranges: TimestampRange[];
  char_count: number;
  token_count: number;
  source_path: string;
  status: string;
}

export interface CourseIndexResponse {
  status: string;
  source_id: string;
  manifest?: unknown;
  indexes: CourseDateIndex[];
  error?: string;
}

export interface RetrievalTrace {
  section: string;
  matches: {
    date: string;
    score: number;
    timestamp_ranges: TimestampRange[];
  }[];
  skipped_reason?: string;
}

export interface CreateProcessResponse {
  status: string;
  process_id?: string;
  job_id?: string;
  error?: string;
}

export interface ReviseResponse {
  status: string;
  markdown: string;
  diff: string;
  has_base_note: boolean;
  diff_updated: boolean;
  error?: string;
}

// ---------------------------------------------------------------------------
// Existing types
// ---------------------------------------------------------------------------
export interface HealthResponse {
  version: string;
  project_dir: string;
  config: Record<string, unknown>;
  llm_available: boolean;
  typst_compiler: string;
  latex_compiler: string;
  pdf_renderer: string;
}

export type AppState = HealthResponse;

export interface OutputFile {
  name: string;
  path: string;
  size: number;
  ext: string;
}

export interface OutputsResponse {
  outputs: Record<string, OutputFile[]>;
}

export interface SecretStatus {
  llm: {
    api_key_set: boolean;
    api_key_masked: string;
    base_url: string;
    model: string;
  };
  canvas: {
    token_set: boolean;
    token_masked: string;
    cookie_set: boolean;
    cookie_masked: string;
  };
  jaccount: {
    cookie_set: boolean;
    cookie_masked: string;
  };
}

export interface SecretsResponse {
  status: string;
  secrets: SecretStatus;
  error?: string;
}

export interface JobInfo {
  job_id: string;
  name: string;
  status: 'pending' | 'running' | 'succeeded' | 'failed';
  logs: string[];
  errors: string[];
  artifact_paths: string[];
  result?: unknown;
  created_at: number;
  finished_at?: number;
}

export interface JobsResponse {
  jobs: JobInfo[];
}

export interface TranscriptFile {
  name: string;
  size: number;
  video_title: string;
  segment_count: number;
  date: string;
}

export interface TranscriptsStatus {
  exists: boolean;
  count: number;
  files: TranscriptFile[];
}

export interface VideoInfo {
  video_id: string;
  title: string;
  duration: number;
  course_begin_time: string;
  course_end_time: string;
  teacher: string;
  classroom: string;
}

export interface DiffHunk {
  base_start: number;
  base_count: number;
  patched_start: number;
  patched_count: number;
  lines: DiffLine[];
}

export interface DiffLine {
  kind: 'context' | 'remove' | 'add';
  base_line?: number;
  patched_line?: number;
  content: string;
}

// ---------------------------------------------------------------------------
// LLM log types
// ---------------------------------------------------------------------------

export interface LlmLogMeta {
  id: string;
  created_at: string;
  finished_at: string;
  duration_ms: number;
  status: string;
  kind: string;
  model: string;
  temperature: number;
  max_tokens: number;
  response_format?: string;
  error?: string;
  preview?: string;
}

export interface LlmLogsResponse {
  logs: LlmLogMeta[];
  error?: string;
}

class ApiClient {
  private base: string;

  constructor() {
    this.base = '';
  }

  private async fetch<T>(url: string, init?: RequestInit): Promise<T> {
    const res = await fetch(`${this.base}${url}`, {
      headers: { 'Content-Type': 'application/json', ...init?.headers },
      ...init,
    });
    if (!res.ok) {
      const body = await res.text().catch(() => '');
      throw new Error(`HTTP ${res.status}: ${body}`);
    }
    return res.json();
  }

  // State
  async getState(): Promise<AppState> {
    return this.fetch('/api/state');
  }

  async patchState(fields: Record<string, unknown>): Promise<unknown> {
    return this.fetch('/api/state', {
      method: 'PATCH',
      body: JSON.stringify({ fields }),
    });
  }

  // Secrets
  async getSecrets(): Promise<SecretsResponse> {
    return this.fetch('/api/secrets');
  }

  async patchSecrets(
    fields: Record<string, string>,
    clear: string[] = [],
  ): Promise<SecretsResponse> {
    return this.fetch('/api/secrets', {
      method: 'PATCH',
      body: JSON.stringify({ fields, clear }),
    });
  }

  // Outputs
  async getOutputs(): Promise<OutputsResponse> {
    return this.fetch('/api/outputs');
  }

  // Jobs
  async getJobs(limit = 30): Promise<JobsResponse> {
    return this.fetch(`/api/jobs?limit=${limit}`);
  }

  async getJob(jobId: string): Promise<JobInfo> {
    return this.fetch(`/api/jobs/${jobId}`);
  }

  // Legacy job endpoint
  async getJobLegacy(jobId: string): Promise<JobInfo> {
    return this.fetch(`/jobs/${jobId}`);
  }

  // Canvas
  async listVideos(courseId: string, cookie: string): Promise<{ job_id: string }> {
    return this.fetch('/api/canvas/list-videos', {
      method: 'POST',
      body: JSON.stringify({ course_id: courseId, cookie }),
    });
  }

  async fetchSubtitles(params: {
    course_id: string;
    cookie: string;
    scope?: string;
    date?: string;
    video_ids?: string[];
    transcripts_dir?: string;
  }): Promise<{ job_id: string }> {
    return this.fetch('/api/canvas/fetch-subtitles', {
      method: 'POST',
      body: JSON.stringify(params),
    });
  }

  // Transcripts
  async getTranscriptsStatus(
    courseId?: string,
    transcriptsDir?: string,
  ): Promise<TranscriptsStatus> {
    const params = new URLSearchParams();
    if (courseId) params.set('course_id', courseId);
    if (transcriptsDir) params.set('transcripts_dir', transcriptsDir);
    return this.fetch(`/api/transcripts/status?${params}`);
  }

  // Notes
  async completeNotes(params: {
    notes_path: string;
    transcripts_dir: string;
    output_notes: string;
    output_patches: string;
  }): Promise<{ job_id: string }> {
    return this.fetch('/api/notes/complete', {
      method: 'POST',
      body: JSON.stringify(params),
    });
  }

  async getNotesDiff(base: string, patched: string): Promise<{
    base: string;
    patched: string;
    hunks: DiffHunk[];
    unified: string;
  }> {
    const params = new URLSearchParams({ base, patched });
    return this.fetch(`/api/notes/diff?${params}`);
  }

  // Sources
  async getSources(): Promise<SourcesResponse> {
    return this.fetch('/api/sources');
  }

  async getSource(id: string): Promise<SourceRecord> {
    return this.fetch(`/api/sources/${id}`);
  }

  async deleteSource(id: string): Promise<SourceActionResponse> {
    return this.fetch(`/api/sources/${id}`, { method: 'DELETE' });
  }

  async createNoteSource(name: string, content: string): Promise<SourceActionResponse> {
    return this.fetch('/api/sources/note', {
      method: 'POST',
      body: JSON.stringify({ name, content }),
    });
  }

  async updateNoteSource(id: string, name: string, content: string): Promise<SourceActionResponse> {
    return this.fetch(`/api/sources/${id}/note`, {
      method: 'PUT',
      body: JSON.stringify({ name, content }),
    });
  }

  async createTranscriptDaySource(params: {
    course_id: string;
    course_name?: string;
    date: string;
    processing_language?: string;
  }): Promise<SourceActionResponse> {
    return this.fetch('/api/sources/transcript-day', {
      method: 'POST',
      body: JSON.stringify(params),
    });
  }

  async createTranscriptCourseSource(params: {
    course_id: string;
    course_name?: string;
    processing_language?: string;
  }): Promise<SourceActionResponse> {
    return this.fetch('/api/sources/transcript-course', {
      method: 'POST',
      body: JSON.stringify(params),
    });
  }

  async syncSource(id: string): Promise<SourceActionResponse> {
    return this.fetch(`/api/sources/${id}/sync`, { method: 'POST' });
  }

  async reindexSource(id: string): Promise<SourceActionResponse> {
    return this.fetch(`/api/sources/${id}/reindex`, { method: 'POST' });
  }

  async getSourceIndex(id: string): Promise<CourseIndexResponse> {
    return this.fetch(`/api/sources/${id}/index`);
  }

  async askSource(id: string, question: string): Promise<AskResponse> {
    return this.fetch(`/api/sources/${id}/ask`, {
      method: 'POST',
      body: JSON.stringify({ question }),
    });
  }

  // Canvas courses
  async getCanvasCourses(token?: string): Promise<CanvasCoursesResponse> {
    const params = token ? `?token=${encodeURIComponent(token)}` : '';
    return this.fetch(`/api/canvas/courses${params}`);
  }

  // Canvas course dates
  async getCourseDates(courseId: string): Promise<CourseDatesResponse> {
    return this.fetch(
      `/api/canvas/course-dates?course_id=${encodeURIComponent(courseId)}`,
    );
  }

  // Streaming ask URL builder (for EventSource)
  askStreamUrl(sourceId: string, question: string): string {
    return `/api/sources/${encodeURIComponent(sourceId)}/ask-stream?question=${encodeURIComponent(question)}`;
  }

  // LLM logs
  async getLlmLogs(limit = 100): Promise<LlmLogsResponse> {
    return this.fetch(`/api/llm-logs?limit=${limit}`);
  }

  async getLlmLog(logId: string): Promise<Record<string, unknown>> {
    return this.fetch(`/api/llm-logs/${encodeURIComponent(logId)}`);
  }

  // Processes
  async getProcesses(): Promise<ProcessesResponse> {
    return this.fetch('/api/processes');
  }

  async getProcess(id: string): Promise<ProcessRecord> {
    return this.fetch(`/api/processes/${id}`);
  }

  async createProcess(params: {
    title?: string;
    source_ids: string[];
    outputs: { kind: string; max_pages?: number }[];
  }): Promise<CreateProcessResponse> {
    return this.fetch('/api/processes', {
      method: 'POST',
      body: JSON.stringify(params),
    });
  }

  async getProcessOutput(
    processId: string,
    outputId: string,
  ): Promise<ProcessOutputContent> {
    return this.fetch(`/api/processes/${processId}/outputs/${outputId}`);
  }

  async addProcessOutput(
    processId: string,
    kind: string,
    maxPages?: number,
  ): Promise<{ status: string; job_id?: string; error?: string }> {
    return this.fetch(`/api/processes/${processId}/outputs`, {
      method: 'POST',
      body: JSON.stringify({ kind, max_pages: maxPages }),
    });
  }

  async deleteProcessOutput(
    processId: string,
    outputId: string,
  ): Promise<{ status: string; process_removed?: boolean; error?: string }> {
    return this.fetch(`/api/processes/${processId}/outputs/${outputId}`, {
      method: 'DELETE',
    });
  }

  async reviseProcessOutput(
    processId: string,
    outputId: string,
    instruction: string,
  ): Promise<ReviseResponse> {
    return this.fetch(
      `/api/processes/${processId}/outputs/${outputId}/revise`,
      {
        method: 'POST',
        body: JSON.stringify({ instruction }),
      },
    );
  }
}

export const api = new ApiClient();
