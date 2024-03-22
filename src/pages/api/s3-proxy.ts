import { NextApiRequest, NextApiResponse } from "next";
import AWS from "aws-sdk";

const s3 = new AWS.S3({
  endpoint: new AWS.Endpoint(process.env.S3_UPLOAD_ENDPOINT!),
  accessKeyId: process.env.S3_UPLOAD_KEY,
  secretAccessKey: process.env.S3_UPLOAD_SECRET,
  s3ForcePathStyle: true,
  signatureVersion: "v4",
  region: process.env.S3_UPLOAD_REGION,
});

export default async function handler(
  req: NextApiRequest,
  res: NextApiResponse,
) {
  const { url } = req.query;

  if (!url || typeof url !== "string") {
    res.status(400).json({ error: "Missing or invalid URL parameter" });
    return;
  }

  try {
    const params = {
      Bucket: process.env.S3_UPLOAD_BUCKET,
      Key: url,
    };

    const response = await s3.getObject(params).promise();

    res.setHeader("Content-Type", response.ContentType);
    res.setHeader("Content-Length", response.ContentLength);
    res.setHeader("Last-Modified", response.LastModified);
    res.setHeader("ETag", response.ETag);
    res.setHeader("Cache-Control", "max-age=31536000, immutable");

    res.status(200).send(response.Body);
  } catch (error) {
    console.error("Error fetching object from S3:", error);
    res.status(500).json({ error: "Failed to fetch object from S3" });
  }
}
