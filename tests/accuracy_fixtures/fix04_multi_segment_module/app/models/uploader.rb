class Uploader
  def client
    @client ||= Aws::S3::Client.new(region: "us-east-1")
  end

  def upload(bucket, key, body)
    client.put_object(bucket: bucket, key: key, body: body)
  end
end

class BatchLoader
  def self.for(model)
    GraphQL::Batch::Loader.for(model)
  end
end
