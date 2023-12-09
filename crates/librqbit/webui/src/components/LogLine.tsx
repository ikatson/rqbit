import React from "react";
import { JSONLogLine, Span } from "../api-types";

const SpanFields: React.FC<{ span: Span }> = ({ span }) => {
  let fields = Object.entries(span).filter(([name, value]) => name != "name");
  if (fields.length == 0) {
    return null;
  }
  return (
    <>
      {"{"}
      {fields
        .map(([name, value]) => {
          return (
            <span key={name}>
              {name} = {value}
            </span>
          );
        })
        .reduce((prev, curr) => (
          <>
            {prev}, {curr}
          </>
        ))}
      {"}"}
    </>
  );
};

const LogSpan: React.FC<{ span: Span }> = ({ span }) => (
  <>
    <span className="fw-bold">{span.name}</span>
    <SpanFields span={span} />
  </>
);

const Fields: React.FC<{ fields: JSONLogLine["fields"] }> = ({ fields }) => (
  <span
    className={`m-1 ${
      fields.message.match(/error|fail/g) ? "text-danger" : "text-muted"
    }`}
  >
    {fields.message}
    {Object.entries(fields)
      .filter(([key, value]) => key != "message")
      .map(([key, value]) => (
        <span className="m-1" key={key}>
          <span className="fst-italic fw-bold">{key}</span>={value}
        </span>
      ))}
  </span>
);

export const LogLine: React.FC<{ line: JSONLogLine }> = React.memo(
  ({ line }) => {
    const parsed = line;

    const classNameByLevel = (level: string) => {
      switch (level) {
        case "DEBUG":
          return "text-primary";
        case "INFO":
          return "text-success";
        case "WARN":
          return "text-warning";
        case "ERROR":
          return "text-danger";
        default:
          return "text-muted";
      }
    };

    return (
      <p className="font-monospace m-0 text-break" style={{ fontSize: "10px" }}>
        <span className="m-1">{parsed.timestamp}</span>
        <span className={`m-1 ${classNameByLevel(parsed.level)}`}>
          {parsed.level}
        </span>

        <span className="m-1">
          {parsed.spans?.map((span, i) => <LogSpan key={i} span={span} />)}
        </span>
        <span className="m-1 text-muted">{parsed.target}</span>
        <Fields fields={parsed.fields} />
      </p>
    );
  }
);
