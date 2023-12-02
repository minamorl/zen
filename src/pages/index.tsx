import { useEffect, useState } from "react";
import { trpc } from "../utils/trpc";
import { SubmitHandler, useForm } from "react-hook-form";
import Link from "next/link";
import { format, formatDistance, parseISO } from "date-fns";
import Uppy from "@uppy/core";
import { Dashboard } from "@uppy/react";
import Tus from "@uppy/tus";

import "@uppy/core/dist/style.min.css";
import "@uppy/dashboard/dist/style.min.css";
import { randomUUID } from "crypto";
import { usePersona } from "../context/personaContext";

type Inputs = {
  raw_text: string;
};

const SUPABASE_ANON_KEY =
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNwaXd4bXRrbXltcWx0ZGZ4b2dnIiwicm9sZSI6ImFub24iLCJpYXQiOjE3MDEyNjk1NzMsImV4cCI6MjAxNjg0NTU3M30.aYkKWGpKvfoOj84p5T4lED0Iy6VXK8yH3Jo0h2kEQMc";
const DEFAULT_BOARD = "test";

const uppy = new Uppy().use(Tus, {
  endpoint: `https://spiwxmtkmymqltdfxogg.supabase.co/storage/v1/upload/resumable`,
  headers: {
    authorization: `Bearer ${SUPABASE_ANON_KEY}`,
    apikey: SUPABASE_ANON_KEY,
  },
  uploadDataDuringCreation: true,
  chunkSize: 6 * 1024 * 1024,
  allowedMetaFields: [
    "bucketName",
    "objectName",
    "contentType",
    "cacheControl",
  ],
});

uppy.on("file-added", (file) => {
  const supabaseMetadata = {
    bucketName: "images",
    objectName: file.name,
    contentType: file.type,
  };

  file.meta = {
    ...file.meta,
    ...supabaseMetadata,
  };

  console.log("file added", file);
});

export default function IndexPage() {
  const [persona] = usePersona();
  const { data, isLoading, mutate } = trpc.createPost.useMutation();
  const [key, setKey] = useState("invalid");
  const [submitting, setSubmitting] = useState(false);
  const { handleSubmit, register } = useForm<Inputs>();
  const {
    data: board,
    refetch,
    isRefetching,
  } = trpc.getBoard.useQuery({
    name: "test",
  });
  const { data: me } = trpc.getPersonas.useQuery();
  console.log(me);
  const [selectedPost, setSelectedPost] = useState("");
  const { mutate: createThread } = trpc.createThread.useMutation();
  const { mutate: createResource } = trpc.createResource.useMutation();

  const [threadInput, setThreadInput] = useState("");

  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    setSubmitting(true);
    const r = await uppy.upload();
    if (!persona) return;

    mutate({
      raw_text: inputs.raw_text,
      persona_id: persona,
    });

    setKey(inputs.raw_text);
    setTimeout(() => {
      refetch().then(() => setSubmitting(false));
    }, 1000);
  };

  if (!board) {
    return (
      <div>
        {me && <div>DEBUG: Your persona id is {persona}</div>}
        <div> Loading...</div>
      </div>
    );
  }
  return (
    <div>
      {me && <div>DEBUG: Your persona id is {persona}</div>}
      <div>
        <div className="shadow p-8 m-4 rounded-xl">
          <h2 className="text-2xl">#{board.name}</h2>
          <div>{board.description}</div>
          <div className="text-cyan-800">
            Created at{" "}
            {formatDistance(parseISO(board.created_at), new Date(), {
              addSuffix: true,
            })}
          </div>

          <div className="border-t-2 border-gray-200 my-4"></div>
          <form onSubmit={handleSubmit(onSubmit)}>
            <textarea
              className="rounded border-l-1 border-black w-full h-full resize-none focus:outline-none"
              {...register("raw_text")}
            />
            <input
              className="bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full"
              type="submit"
              value="Post"
            />
            {null && <Dashboard uppy={uppy} hideUploadButton />}
          </form>
        </div>
      </div>
      <div>
        <ul>
          {submitting && (
            <li key="submitting" className="shadow p-8 m-4 rounded opacity-50">
              <div>{key}</div>
              <div className="text-cyan-800">
                Created at{" "}
                {formatDistance(Date.now(), new Date(), {
                  addSuffix: true,
                })}
              </div>
            </li>
          )}
          {board.posts.map((v) => (
            <>
              <li
                key={v.id}
                className="shadow p-8 m-4 rounded cursor-pointer"
                onClick={() => setSelectedPost(v.id)}
              >
                <div>{v.raw_text}</div>
                <div>{v.resources && v?.resources?.[0]?.path}</div>
                <div>
                  {formatDistance(parseISO(v.created_at), new Date(), {
                    addSuffix: true,
                  })}
                </div>
              </li>
              {v.threads.map((x) => (
                <li key={x.id} className="shadow p-8 m-4 mx-8 rounded">
                  <div>{x.raw_text}</div>
                  <div>
                    {formatDistance(parseISO(x.created_at), new Date(), {
                      addSuffix: true,
                    })}
                  </div>
                </li>
              ))}
              {selectedPost === v.id && (
                <li key="input" className="shadow p-4 m-4 mx-8 rounded">
                  <textarea
                    onChange={(e) => setThreadInput(e.currentTarget.value)}
                    value={threadInput}
                    className="rounded border-l-1 border-black w-full h-full resize-none"
                  />
                  <button
                    onClick={() => {
                      createThread({
                        raw_text: threadInput,
                        post_id: v.id,
                        persona_id: me?.personas?.[0].id ?? "",
                      });
                      window.setTimeout(() => refetch(), 500);
                    }}
                    className="bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full"
                  >
                    Create Thread
                  </button>
                </li>
              )}
            </>
          ))}
        </ul>
      </div>
    </div>
  );
}
