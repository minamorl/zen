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
import { ConsoleUI } from "@/console";
import { useTheme } from "next-themes";
import { useConsole } from "../context/consoleContext";

type Inputs = {
  raw_text: string;
};

const DEFAULT_BOARD = "test";

export default function IndexPage() {
  const [persona] = usePersona();
  const { data, isLoading, mutate } = trpc.createPost.useMutation();
  const [key, setKey] = useState("invalid");
  const [submitting, setSubmitting] = useState(false);
  const [threadSubmitting, setThreadSubmitting] = useState(false);
  const { handleSubmit, register } = useForm<Inputs>();
  const { theme, setTheme } = useTheme();
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

  const [threadInput, setThreadInput] = useState("");
  const [message, setMessage] = useConsole();
  useEffect(() => {
    setMessage("Console is starting up...");
  }, []);

  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    setSubmitting(true);
    setMessage("Submitting post...");
    if (!persona) return;

    mutate({
      raw_text: inputs.raw_text,
      persona_id: persona,
      board_name: DEFAULT_BOARD,
    });

    setKey(inputs.raw_text);
    setTimeout(() => {
      refetch().then(() => setSubmitting(false));
      setMessage("Correctly submitted post!");
    }, 1000);
  };

  if (!board) {
    setMessage("Loading board...");
    setMessage(`Your persona id is ${persona}`);
    return <div></div>;
  }
  return (
    <div className="h-full">
      <div>
        <div className="p-8 m-4 rounded-xl bg-gray-700 bg-opacity-75 shadow-2xl">
          <h2 className="text-2xl">#{board.title}</h2>

          <div className="border-t-2 border-gray-200 my-4"></div>
          <form onSubmit={handleSubmit(onSubmit)}>
            <textarea
              className="rounded border-l-1 border-black w-full h-full resize-none focus:outline-none bg-transparent bg-opacity-100 "
              rows={5}
              {...register("raw_text")}
            />
            <input
              className="bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full"
              type="submit"
              value="Post"
            />
          </form>
        </div>
      </div>
      <div>
        <ul>
          {submitting && (
            <li
              key="submitting"
              className="shadow-2xl p-8 m-4 rounded opacity-50 bg-gray-700 bg-opacity-75"
            >
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
                className="shadow-2xl p-8 m-4 rounded cursor-pointer bg-gray-700 bg-opacity-75"
                onClick={() => setSelectedPost(v.id)}
              >
                <div>{v.content}</div>
              </li>
              {v.threads.map((x) => (
                <li
                  key={x.id}
                  className="shadow-2xl p-8 m-4 mx-8 rounded bg-gray-700 bg-opacity-75"
                >
                  <div>{x.content}</div>
                </li>
              ))}

              {threadSubmitting && (
                <li
                  key="thread-submitting"
                  className="shadow-2xl p-8 m-4 mx-8 rounded opacity-50 bg-gray-700 bg-opacity-75"
                >
                  <div>{threadInput}</div>
                </li>
              )}
              {selectedPost === v.id && (
                <li
                  key="thread-input"
                  className="shadow-2xl p-4 m-4 mx-8 rounded bg-gray-700 bg-opacity-75"
                >
                  <textarea
                    onChange={(e) => setThreadInput(e.currentTarget.value)}
                    value={threadInput}
                    className="rounded border-l-1 border-black w-full h-full resize-none bg-transparent bg-opacity-75"
                  />
                  <button
                    onClick={() => {
                      setSelectedPost("");

                      setThreadSubmitting(true);
                      createThread({
                        raw_text: threadInput,
                        post_id: v.id,
                        persona_id: persona,
                      });
                      window.setTimeout(() => {
                        const r = refetch();
                        r.then(() => setThreadSubmitting(false));
                      }, 1000);
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
