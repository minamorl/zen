import quill from "quill";
import React, { useEffect, useRef } from "react";

const Editor: React.FC = () => {
  const editorRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (editorRef.current) {
      new quill(editorRef.current, {
        theme: "snow",
        placeholder: "Write something awesome...",
      });
    }
  }, [editorRef]);

  return (
    <div className="quill">
      <div ref={editorRef} />
    </div>
  );
};

export default Editor;
