import { useEffect, useState } from 'react';
import { api, type AppState, type OutputFile } from '../api';

export default function Dashboard() {
  const [state, setState] = useState<AppState | null>(null);
  const [outputs, setOutputs] = useState<Record<string, OutputFile[]>>({});
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const [s, o] = await Promise.all([api.getState(), api.getOutputs()]);
        if (!cancelled) {
          setState(s);
          setOutputs(o.outputs);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) {
          setError(String(e));
        }
      }
    }
    load();
    return () => {
      cancelled = true;
    };
  }, []);

  if (error) {
    return (
      <div className="error-block">
        Failed to load state: {error}
        <p className="note mt-4">
          Is the lecture-distill server running? Try{' '}
          <code>cargo run -- gui</code>
        </p>
      </div>
    );
  }

  if (!state) {
    return <p className="text-muted">Loading...</p>;
  }

  const artifactSummary = Object.entries(outputs).map(([category, files]) => (
    <tr key={category}>
      <th>{category}</th>
      <td>{files.length} file(s)</td>
    </tr>
  ));

  const configEntries = Object.entries(state.config).filter(
    ([, v]) => v !== '' && v !== null,
  );

  return (
    <div className="card-grid">
      <div className="card">
        <h2>Environment</h2>
        <table className="info-table">
          <tbody>
            <tr>
              <th>Version</th>
              <td>{state.version}</td>
            </tr>
            <tr>
              <th>Project Dir</th>
              <td>
                <code className="text-sm">{state.project_dir}</code>
              </td>
            </tr>
            <tr>
              <th>LLM</th>
              <td>
                {state.llm_available ? (
                  <span className="badge badge-ok">Available</span>
                ) : (
                  <span className="badge badge-warn">Not Set</span>
                )}
              </td>
            </tr>
            <tr>
              <th>PDF Renderer</th>
              <td>
                {state.pdf_renderer ? (
                  <span className="badge badge-ok">{state.pdf_renderer}</span>
                ) : (
                  <span className="badge badge-warn">Not Found</span>
                )}
              </td>
            </tr>
            <tr>
              <th>Typst</th>
              <td>
                {state.typst_compiler ? (
                  <span className="badge badge-ok">{state.typst_compiler}</span>
                ) : (
                  <span className="badge badge-warn">Not Found</span>
                )}
              </td>
            </tr>
            <tr>
              <th>LaTeX Fallback</th>
              <td>
                {state.latex_compiler ? (
                  <span className="badge badge-ok">{state.latex_compiler}</span>
                ) : (
                  <span className="badge badge-warn">Not Found</span>
                )}
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <div className="card">
        <h2>Configuration</h2>
        {configEntries.length === 0 ? (
          <p className="text-muted text-sm">No configuration set.</p>
        ) : (
          <table className="info-table">
            <tbody>
              {configEntries.map(([key, value]) => (
                <tr key={key}>
                  <th>{key}</th>
                  <td>
                    <code className="text-sm">{String(value)}</code>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <div className="card">
        <h2>Artifact Summary</h2>
        {artifactSummary.length === 0 ? (
          <p className="text-muted text-sm">
            No artifacts yet. Use Sources &amp; Outputs to generate content.
          </p>
        ) : (
          <table className="info-table">
            <tbody>{artifactSummary}</tbody>
          </table>
        )}
      </div>
    </div>
  );
}
