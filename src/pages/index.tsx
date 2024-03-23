import { SetStateAction, useEffect, useState, Dispatch } from "react";
import { trpc } from "../utils/trpc";
import { SubmitHandler, useForm } from "react-hook-form";
import { formatDistance, formatDistanceToNow, parseISO } from "date-fns";

import "@uppy/core/dist/style.min.css";
import "@uppy/dashboard/dist/style.min.css";
import { usePersona } from "../context/personaContext";
import { useConsole } from "../context/consoleContext";
import { GoCommandPalette } from "react-icons/go";
import { animated, useSpring } from "@react-spring/web";
import Dragndrop from "../dragndrop/dragndrop";
import { Prisma } from "@prisma/client";
import Image from "next/image";

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

const PostForm = ({ onSubmit }: { onSubmit: SubmitHandler<Inputs> }) => {
  const [persona] = usePersona();
  const [message, setMessage] = useConsole();
  const { handleSubmit, register } = useForm<Inputs>();

  return (
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
  );
};

interface BoardPostProps {
  post: Prisma.PostGetPayload<{
    include: {
      attachment: true;
    };
  }>;
  setSelectedPost: (postId: string) => void;
}
const BoardPost: React.FC<BoardPostProps> = ({ post, setSelectedPost }) => {
  const styles = useSpring({
    from: { opacity: 0, transform: "translate3d(0, -40px, 0)" },
    to: { opacity: 1, transform: "translate3d(0, 0, 0)" },
  });

  const s3url =
    post.attachment?.url.replace("http://s3:9000/zen/", "").split("?")[0] ??
    null;
  const proxyUrl = `/api/s3-proxy?url=${s3url}`;
  console.log(proxyUrl);

  return (
    <animated.div key={post.id} style={styles}>
      <li
        className="shadow-2xl p-8 m-4 rounded cursor-pointer bg-gray-700 bg-opacity-75"
        onClick={() => setSelectedPost(post.id)}
      >
        <div>{post.content}</div>
        {post.attachment && (
          <div className="mt-4">
            <Image
              src={proxyUrl}
              alt="attachment image"
              className="w-full"
              width={1200}
              height={600}
            />
          </div>
        )}
        <div className="text-cyan-200">
          Created at{" "}
          {formatDistanceToNow(parseISO(post.createdAt.toString()), {
            addSuffix: true,
          })}
        </div>
      </li>
    </animated.div>
  );
};

const PostSubmittingDisplay = ({ text }: { text: string }) => {
  return (
    <li
      key="post-submitting"
      className="shadow-2xl p-8 m-4 rounded bg-gray-700 bg-opacity-75"
    >
      <div>{text}</div>
      <div className="text-cyan-200">
        Created at{" "}
        {formatDistance(Date.now(), new Date(), {
          addSuffix: true,
        })}
      </div>
    </li>
  );
};
export default function IndexPage() {
  const [boardName, setBoardName] = useState(DEFAULT_BOARD);
  const [persona] = usePersona();
  const { mutate } = trpc.createPost.useMutation();
  const [key, setKey] = useState("invalid");
  const [submitting, setSubmitting] = useState(false);
  // undefined means first fetch
  const [lastTimeFetched, setLastTimeFetched] = useState<number | undefined>(
    undefined,
  );
  const {
    data: fetchedBoard,
    refetch,
    isLoading: boardLoading,
  } = trpc.getBoard.useQuery(
    {
      name: boardName,
      lastTimeFetched,
    },
    {
      keepPreviousData: true,
      select: (data) => {
        if (data) {
          return {
            ...data,
            posts: data.posts.map((post) => ({
              ...post,
              attachment: post.attachment || null,
            })),
          };
        }
        return data;
      },
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
  const onPostSubmit: SubmitHandler<Inputs> = async (inputs) => {
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
      <div className="max-w-4xl mx-auto">
        <div>
          <PostForm onSubmit={onPostSubmit} />
        </div>

        <div>
          <ul>
            {submitting && <PostSubmittingDisplay text={key} />}
            {board.posts.map((post) => (
              <BoardPost
                key={post.id}
                post={post}
                setSelectedPost={setSelectedPost}
                createThread={createThread}
                persona={persona}
                refetch={refetch}
              />
            ))}
          </ul>
        </div>
      </div>
      <Dragndrop />
    </div>
  );
}
