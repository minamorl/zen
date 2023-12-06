// pages/api/proxy/[fileKey].js
import { S3 } from "aws-sdk";

export default async function handler(req, res) {
  const { fileKey } = req.query;

  // Initialize S3 with Wasabi credentials
  const s3 = new S3({
    accessKeyId: process.env.AWS_ACCESS_KEY_ID,
    secretAccessKey: process.env.AWS_SECRET_ACCESS_KEY,
    endpoint: "s3.ap-northeast-1.wasabisys.com", // Use the appropriate Wasabi endpoint
    s3ForcePathStyle: true,
    signatureVersion: "v4",
  });

  try {
    // Fetch the file from Wasabi
    const fileStream = s3
      .getObject({
        Bucket: process.env.AWS_BUCKET_NAME!,
        Key: fileKey,
      })
      .createReadStream();

    // Pipe the file stream directly to the response
    fileStream.pipe(res);
  } catch (error) {
    console.error(error);
    res.status(500).send("Error retrieving file");
  }
}
