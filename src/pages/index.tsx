import { SetStateAction, useEffect, useState, Dispatch } from "react";
import { trpc } from "../utils/trpc";
import { SubmitHandler, useForm } from "react-hook-form";
import Link from "next/link";
import {
  format,
  formatDistance,
  formatDistanceToNow,
  parseISO,
} from "date-fns";
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
import { GoCommandPalette } from "react-icons/go";
type Inputs = {
  raw_text: string;
};

const DEFAULT_BOARD = "test";

const Title = ({
  boardName,
  setBoardName,
}: {
  boardName: string;
  setBoardName: Dispatch<SetStateAction<string>>;
}) => {
  const [value, setValue] = useState(boardName);
  return (
    <div className="ml-4 border-gray-800 border-l-1 font-mono">
      <GoCommandPalette className="inline-block mr-2 " />
      <input
        className="bg-transparent border-none focus:outline-none"
        value={value}
        onChange={(e) => {
          const value = e.currentTarget.value;
          setValue(value);
          setBoardName(value);
        }}
      />
    </div>
  );
};

export default function IndexPage() {
  const [boardName, setBoardName] = useState(DEFAULT_BOARD);
  const [persona] = usePersona();
  const { mutate } = trpc.createPost.useMutation();
  const [key, setKey] = useState("invalid");
  const [submitting, setSubmitting] = useState(false);
  const [threadSubmitting, setThreadSubmitting] = useState(false);
  const { handleSubmit, register } = useForm<Inputs>();
  // undefined means first fetch
  const [lastTimeFetched, setLastTimeFetched] = useState<number | undefined>(
    undefined,
  );
  const {
    data: fetchedBoard,
    refetch,
    isLoading: boardLoading,
    error: boardFetchError,
  } = trpc.getBoard.useQuery(
    {
      name: boardName,
      lastTimeFetched,
    },
    {
      keepPreviousData: true,
    },
  );
  const [board, setBoard] = useState(fetchedBoard);
  const [selectedPost, setSelectedPost] = useState("");
  const { mutate: createThread } = trpc.createThread.useMutation();

  const [threadInput, setThreadInput] = useState("");
  const [message, setMessage] = useConsole();
  useEffect(() => {
    const interval = setInterval(() => {
      // 6 seconds ago for resolving delay
      setLastTimeFetched(Date.now() - 6000);
    }, 6000);
    return () => clearInterval(interval);
  }, []);
  useEffect(() => {
    setMessage("You are in #" + boardName + "!");
    setLastTimeFetched(undefined);
    setBoard(undefined);
  }, [boardName]);

  useEffect(() => {
    if (!fetchedBoard) return;

    setBoard((prev) => {
      if (lastTimeFetched && prev) {
        const newPosts = fetchedBoard.posts;
        const existingPosts = prev.posts;

        // Merge and remove duplicates
        const combinedPosts = newPosts
          .concat(existingPosts)
          .reduce<any>((acc, current) => {
            const x = acc.find((item: any) => item.id === current.id);
            if (!x) {
              return acc.concat([current]);
            } else {
              return acc;
            }
          }, []);

        return { ...fetchedBoard, posts: combinedPosts };
      } else {
        return fetchedBoard;
      }
    });
  }, [fetchedBoard, lastTimeFetched]);
  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    setLastTimeFetched(Date.now());
    setSubmitting(true);
    setMessage("Submitting post...");
    if (!persona) return;

    mutate({
      raw_text: inputs.raw_text,
      persona_id: persona,
      board_name: boardName,
    });

    setKey(inputs.raw_text);
    setTimeout(() => {
      refetch().then(() => setSubmitting(false));
      setMessage("Correctly submitted post!");
    }, 1000);
  };
  const { mutate: createBoard } = trpc.createBoard.useMutation();
  if (!board && !boardLoading) {
    return (
      <div className="h-full w-full">
        <Title boardName={boardName} setBoardName={setBoardName} />
        <div>
          This board does not exist. Do you want to create it?
          <button
            onClick={() => {
              createBoard({ name: boardName });
              refetch();
            }}
            className="bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full"
          >
            Create
          </button>
        </div>
      </div>
    );
  }
  if (!board) {
    // loading animation
    return (
      <div>
        <Title boardName={boardName} setBoardName={setBoardName} />
        <div className="p-8 m-4 rounded-xl bg-gray-700 h-12 bg-opacity-75 shadow-2xl animate-pulse" />
      </div>
    );
  }
  return (
    <div className="h-full w-full">
      <Title boardName={boardName} setBoardName={setBoardName} />
      <div>
        <div
          className="p-8 m-4 rounded-xl bg-gray-700 bg-opacity-75 shadow-2xl"
          onMouseOver={() => {
            persona || setMessage("Please log in first to post.");
          }}
          onKeyDown={(e) => {
            if (e.ctrlKey && e.key === "Enter") {
              handleSubmit(onSubmit)();
            }
          }}
        >
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
              key="post-submitting"
              className="shadow-2xl p-8 m-4 rounded bg-gray-700 bg-opacity-75"
            >
              <div>{key}</div>
              <div className="text-cyan-200">
                Created at{" "}
                {formatDistance(Date.now(), new Date(), {
                  addSuffix: true,
                })}
              </div>
            </li>
          )}
          {board.posts.map((v) => (
            <div key={v.id}>
              <li
                key={"post-" + v.id}
                className="shadow-2xl p-8 m-4 rounded cursor-pointer bg-gray-700 bg-opacity-75"
                onClick={() => setSelectedPost(v.id)}
              >
                <div>{v.content}</div>
                <div className="text-cyan-200">
                  Created at{" "}
                  {formatDistanceToNow(parseISO(v.createdAt), {
                    addSuffix: true,
                  })}
                </div>
              </li>
              {v.threads.map((x) => (
                <li
                  key={"thread-" + x.id}
                  className="shadow-2xl p-8 m-4 mx-8 rounded bg-gray-700 bg-opacity-75"
                >
                  <div>{x.content}</div>
                  <div className="text-cyan-200">
                    Created at{" "}
                    {formatDistanceToNow(parseISO(v.createdAt), {
                      addSuffix: true,
                    })}
                  </div>
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
            </div>
          ))}
        </ul>
      </div>
    </div>
  );
}
