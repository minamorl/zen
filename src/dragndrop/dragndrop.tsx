import React, { useEffect, useState } from "react";
import { trpc } from "../utils/trpc";
import { usePersona } from "../context/personaContext";

const DragDropArea = () => {
  const [isDragging, setIsDragging] = useState(false);
  const createPost = trpc.createPost.useMutation();
  const [persona] = usePersona();

  useEffect(() => {
    const handleDragEnter = (event: any) => {
      event.preventDefault();
      event.stopPropagation();
      setIsDragging(true);
    };

    const handleDragOver = (event: any) => {
      event.preventDefault();
      event.stopPropagation();
    };

    const handleDragLeave = (event: any) => {
      event.preventDefault();
      event.stopPropagation();
      setIsDragging(false);
    };

    const handleDrop = async (event: any) => {
      event.preventDefault();
      event.stopPropagation();
      setIsDragging(false);

      const files = event.dataTransfer.files;
      console.log(files);

      try {
        const uploadPromises = Array.from(files).map(async (file: any) => {
          const formData = new FormData();
          formData.append("file", file);

          const response = await fetch("/api/upload", {
            method: "POST",
            body: formData,
          });

          if (response.ok) {
            const data = await response.json();
            return data.url;
          } else {
            throw new Error("Error uploading file");
          }
        });

        const uploadResults = await Promise.all(uploadPromises);
        console.log("Files uploaded:", uploadResults);

        // Create a new post with the uploaded file URL
        const postData = {
          persona_id: persona,
          raw_text: "",
          board_name: "test",
          attachment_url: uploadResults[0], // Assuming only one file is uploaded
        };

        const createPostResult = await createPost.mutateAsync(postData);
        console.log("Post created:", createPostResult);

        // Handle the created post as needed
      } catch (error) {
        console.error("Error uploading files or creating post:", error);
      }
    };

    document.addEventListener("dragenter", handleDragEnter);
    document.addEventListener("dragover", handleDragOver);
    document.addEventListener("dragleave", handleDragLeave);
    document.addEventListener("drop", handleDrop);

    return () => {
      document.removeEventListener("dragenter", handleDragEnter);
      document.removeEventListener("dragover", handleDragOver);
      document.removeEventListener("dragleave", handleDragLeave);
      document.removeEventListener("drop", handleDrop);
    };
  }, [createPost, persona]);

  if (!isDragging) {
    return null;
  }

  return (
    <div
      style={{
        position: "fixed",
        top: 0,
        left: 0,
        right: 0,
        bottom: 0,
        backgroundColor: "rgba(0, 0, 0, 0.5)",
        display: "flex",
        justifyContent: "center",
        alignItems: "center",
        zIndex: 9999,
        pointerEvents: "none",
      }}
    ></div>
  );
};

export default DragDropArea;
