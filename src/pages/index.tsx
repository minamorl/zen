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

const DEFAULT_BOARD = "test";

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

  const [threadInput, setThreadInput] = useState("");

  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    setSubmitting(true);
    if (!persona) return;

    mutate({
      raw_text: inputs.raw_text,
      persona_id: persona,
      board_name: DEFAULT_BOARD,
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
          <h2 className="text-2xl">#{board.title}</h2>

          <div className="border-t-2 border-gray-200 my-4"></div>
          <form onSubmit={handleSubmit(onSubmit)}>
            <textarea
              className="rounded border-l-1 border-black w-full h-full resize-none focus:outline-none"
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
                <div>{v.content}</div>
              </li>
              {v.threads.map((x) => (
                <li key={x.id} className="shadow p-8 m-4 mx-8 rounded">
                  <div>{x.content}</div>
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
                      // createThread({
                      //   raw_text: threadInput,
                      //   post_id: v.id,
                      //   persona_id: me?.personas?.[0].id ?? "",
                      // });
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
