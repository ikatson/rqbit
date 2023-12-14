import React, { useCallback, useEffect, useRef, useState } from "react";
import { ErrorWithLabel } from "../rqbit-web";
import { ErrorComponent } from "./ErrorComponent";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import debounce from "lodash.debounce";
import { LogLine } from "./LogLine";
import { JSONLogLine } from "../api-types";
import { Form } from "./forms/Form";
import { FormInput } from "./forms/FormInput";

interface LogStreamProps {
  url: string;
  maxLines?: number;
}

export interface Line {
  id: number;
  content: string;
  parsed: JSONLogLine;
  show: boolean;
}

const mergeBuffers = (a1: Uint8Array, a2: Uint8Array): Uint8Array => {
  if (a1.length === 0) {
    return a2;
  }
  if (a2.length === 0) {
    return a1;
  }
  const merged = new Uint8Array(a1.length + a2.length);
  merged.set(a1);
  merged.set(a2, a1.length);
  return merged;
};

const streamLogs = (
  url: string,
  addLine: (text: string) => void,
  setError: (error: ErrorWithLabel | null) => void
): (() => void) => {
  const controller = new AbortController();
  const signal = controller.signal;

  let canceled = false;

  const cancelFetch = () => {
    console.log("cancelling fetch");
    canceled = true;
    controller.abort();
  };

  const runOnce = async () => {
    let response = await fetch(url, { signal });

    if (!response.ok) {
      let text = await response.text();
      setError({
        text: "error fetching logs",
        details: {
          statusText: response.statusText,
          text,
        },
      });
      throw null;
    }

    if (!response.body) {
      setError({
        text: "error fetching logs: ReadableStream not supported.",
      });
      return;
    }

    setError(null);

    const reader = response.body.getReader();

    let buffer = new Uint8Array();
    while (true) {
      const { done, value } = await reader.read();

      if (done) {
        setError({
          text: "log stream terminated",
        });
        throw null;
      }

      buffer = mergeBuffers(buffer, value);

      for (let newLineIdx: number; (newLineIdx = buffer.indexOf(10)) !== -1; ) {
        let lineBytes = buffer.slice(0, newLineIdx);
        let line = new TextDecoder().decode(lineBytes);
        addLine(line);
        buffer = buffer.slice(newLineIdx + 1);
      }
    }
  };

  let cancelLoop = loopUntilSuccess(
    () =>
      runOnce().then(
        () => {},
        (e) => {
          if (canceled) {
            return;
          }
          if (e === null) {
            // We already set the error.
            return;
          }
          setError({
            text: "error streaming logs",
            details: {
              text: e.toString(),
            },
          });
          throw e;
        }
      ),
    1000
  );

  return () => {
    cancelFetch();
    cancelLoop();
  };
};

export const LogStream: React.FC<LogStreamProps> = ({ url, maxLines }) => {
  const [logLines, setLogLines] = useState<Line[]>([]);
  const [error, setError] = useState<ErrorWithLabel | null>(null);
  const [filter, setFilter] = useState<string>("");
  const filterRegex = useRef<RegExp | null>(null);

  const maxL = maxLines ?? 1000;

  const addLine = useCallback(
    (text: string) => {
      setLogLines((logLines: Line[]) => {
        const nextLineId = logLines.length == 0 ? 0 : logLines[0].id + 1;

        let newLogLines = [
          {
            id: nextLineId,
            content: text,
            parsed: JSON.parse(text) as JSONLogLine,
            show: filterRegex.current
              ? !!text.match(filterRegex.current)
              : true,
          },
          ...logLines.slice(0, maxL - 1),
        ];
        return newLogLines;
      });
    },
    [filterRegex.current, maxLines]
  );

  const addLineRef = useRef(addLine);
  addLineRef.current = addLine;

  const updateFilter = debounce((value: string) => {
    let regex: RegExp | null = null;
    try {
      regex = new RegExp(value);
    } catch (e) {
      return;
    }
    filterRegex.current = regex;
    setLogLines((logLines) => {
      let tmp = [...logLines];
      for (let line of tmp) {
        line.show = !!line.content.match(regex as RegExp);
      }
      return tmp;
    });
  }, 200);

  const handleFilterChange = (value: string) => {
    setFilter(value);
    updateFilter(value);
  };

  useEffect(() => updateFilter.cancel, []);

  useEffect(() => {
    return streamLogs(url, (line) => addLineRef.current(line), setError);
  }, [url]);

  return (
    <div>
      <ErrorComponent error={error} />
      <div className="mb-3">
        Showing last {maxL} logs since this window was opened
      </div>
      <Form>
        <FormInput
          value={filter}
          name="filter"
          placeholder="Enter filter (regex)"
          onChange={(e) => handleFilterChange(e.target.value)}
        />
      </Form>

      {logLines.map((line) => (
        <div key={line.id} hidden={!line.show}>
          <LogLine line={line.parsed} />
        </div>
      ))}
    </div>
  );
};
